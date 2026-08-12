// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Native HTTP inference (`/api/{model}`) and the OpenAI embeddings adaptor.
//!
//! The native contract is the JSON the engine executors already speak: a
//! keyed object whose keys follow `executor.inputs[].json_key` (`texts` for
//! embed models, `embeddings` for converters). Responses use the ninference
//! envelope `{success, data}` / `{success, error:{message}}` at HTTP 200, so
//! a dashboard treats the envelope, not the status, as the contract.
//!
//! `/api/openai/embeddings` rewrites an OpenAI `/v1/embeddings` body into
//! that native payload, runs the same path, and reshapes the result. Errors
//! there carry a real HTTP status and the OpenAI `{error:{...}}` shape.

use crate::limits::MAX_REQUEST_ITEMS;
use crate::metrics::{METHOD_CONVERT, METHOD_EMBED};
use crate::state::ServerState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use engine::{EngineError, ExecutorOutput, InputData};
use providers::gateway::{GatewayError, InputType};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use shared::ErrorCode;
use std::sync::Arc;
use std::time::Instant;

/// Bodies without a JSON content type are sniffed as JSON up to this size.
const MAX_JSON_AUTODETECT: usize = 1_048_576;

fn native_ok(data: Value) -> Response {
    Json(json!({ "success": true, "data": data })).into_response()
}

fn native_err(status: StatusCode, message: impl Into<String>) -> Response {
    let body = json!({ "success": false, "error": { "message": message.into() } });
    (status, Json(body)).into_response()
}

/// JSON 404 for unmatched `/api/*` paths, instead of Axum's empty one.
pub async fn api_not_found() -> Response {
    native_err(
        StatusCode::NOT_FOUND,
        "the requested API endpoint does not exist",
    )
}

fn not_loaded(model: &str) -> String {
    format!(
        "model {model:?} is not loaded on this node; put it on disk \
         (`postvec model pull {model}`) and load it with \
         `postvec-server load {model}` — inference requests never load models"
    )
}

pub async fn model_details(
    State(state): State<Arc<ServerState>>,
    Path(model): Path<String>,
) -> Response {
    if let Ok(loaded) = state.engine.get_model(&model) {
        return match loaded.overview() {
            Ok(overview) => native_ok(serde_json::to_value(overview).unwrap_or(Value::Null)),
            Err(e) => native_err(StatusCode::OK, e.to_string()),
        };
    }
    if state.gateway.owns(&model) {
        return native_ok(json!({ "inputs": [], "outputs": [], "provider": true }));
    }
    native_err(StatusCode::OK, not_loaded(&model))
}

pub async fn predict(
    State(state): State<Arc<ServerState>>,
    Path(model): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let input = match detect_input(content_type, &body) {
        Ok(input) => input,
        Err(e) => return native_err(StatusCode::OK, e.message()),
    };
    match run_predict(&state, &model, input, headers).await {
        Ok(Output::Json(data)) => native_ok(data),
        Ok(Output::Binary { data, content_type }) => {
            let content_type = content_type
                .parse()
                .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream"));
            ([(header::CONTENT_TYPE, content_type)], data).into_response()
        }
        Err(e) => native_err(StatusCode::OK, e.message()),
    }
}

#[derive(Deserialize)]
pub struct ConvertPayload {
    source_model: String,
    target_model: String,
    embeddings: Value,
}

/// `POST /api/convert`: the SQL `convert(embedding, source, target)` over
/// HTTP, resolving the converter from the pair the way `migrate()` does.
pub async fn convert(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(payload): Json<ConvertPayload>,
) -> Response {
    let envelope = crate::http::config_envelope(&state).await;
    let models =
        postvec_core::client::discovery::parse_config(&envelope.to_string()).unwrap_or_default();
    let Some(model) = converter_for(&models, &payload.source_model, &payload.target_model) else {
        return native_err(
            StatusCode::NOT_FOUND,
            format!(
                "no converter from {:?} to {:?} on this node",
                payload.source_model, payload.target_model
            ),
        );
    };
    let input = InputData::Json(json!({ "embeddings": payload.embeddings }));
    match run_predict(&state, &model, input, headers).await {
        Ok(Output::Json(data)) => native_ok(data),
        Ok(Output::Binary { .. }) => native_err(StatusCode::OK, "converter returned binary output"),
        Err(e) => native_err(StatusCode::OK, e.message()),
    }
}

/// A local converter for the pair wins over a provider-backed one; ties by name.
fn converter_for(
    models: &[postvec_core::client::ModelInfo],
    source: &str,
    target: &str,
) -> Option<String> {
    models
        .iter()
        .filter(|m| {
            m.model_type == "convert"
                && m.source_model.as_deref() == Some(source)
                && m.target_model.as_deref() == Some(target)
        })
        .min_by_key(|m| (!m.raw["extra"]["provider"].is_null(), m.name.clone()))
        .map(|m| m.name.clone())
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    NotLoaded(String),
    Engine(EngineError),
    Gateway(GatewayError),
    Internal(String),
}

impl ApiError {
    fn message(&self) -> String {
        match self {
            ApiError::BadRequest(m) | ApiError::NotLoaded(m) | ApiError::Internal(m) => m.clone(),
            ApiError::Engine(e) => e.to_string(),
            ApiError::Gateway(e) => e.message.clone(),
        }
    }

    fn metrics_label(&self) -> &str {
        match self {
            ApiError::Engine(e) => e.to_error_code().as_str(),
            ApiError::Gateway(e) => e.code.as_str(),
            ApiError::BadRequest(_) => "INVALID_INPUT",
            ApiError::NotLoaded(_) => "MODEL_NOT_LOADED",
            ApiError::Internal(_) => "INTERNAL",
        }
    }

    /// OpenAI error envelope. The `type` vocabulary and status mapping
    /// follow aphex; unknown models get OpenAI's own `model_not_found`.
    fn into_openai(self) -> Response {
        let (status, kind, code) = match &self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "invalid_request_error", None),
            ApiError::NotLoaded(_) => (
                StatusCode::NOT_FOUND,
                "invalid_request_error",
                Some("model_not_found"),
            ),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "api_error", None),
            ApiError::Engine(e) => openai_class(e.to_error_code()),
            ApiError::Gateway(e) => openai_class(e.code),
        };
        let body = json!({
            "error": {
                "message": self.message(),
                "type": kind,
                "code": code,
                "param": Value::Null,
            }
        });
        (status, Json(body)).into_response()
    }
}

fn openai_class(code: ErrorCode) -> (StatusCode, &'static str, Option<&'static str>) {
    match code {
        ErrorCode::InvalidInput => (StatusCode::BAD_REQUEST, "invalid_request_error", None),
        ErrorCode::ContextLengthExceeded => (
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            Some("context_length_exceeded"),
        ),
        ErrorCode::ModelNotFound | ErrorCode::ModelNotLoaded => (
            StatusCode::NOT_FOUND,
            "invalid_request_error",
            Some("model_not_found"),
        ),
        ErrorCode::Timeout => (StatusCode::GATEWAY_TIMEOUT, "api_error", None),
        ErrorCode::UpstreamAuthFailed => (StatusCode::UNAUTHORIZED, "authentication_error", None),
        ErrorCode::UpstreamServiceUnavailable => (StatusCode::BAD_GATEWAY, "api_error", None),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "api_error", None),
    }
}

fn detect_input(content_type: &str, body: &[u8]) -> Result<InputData, ApiError> {
    let parse = |body: &[u8]| {
        serde_json::from_slice::<Value>(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON payload: {e}")))
    };
    if content_type.starts_with("application/json") {
        return parse(body).map(InputData::Json);
    }
    let binary = [
        "image/",
        "audio/",
        "video/",
        "application/octet-stream",
        "application/pdf",
        "application/zip",
    ];
    if binary.iter().any(|p| content_type.starts_with(p)) || body.len() > MAX_JSON_AUTODETECT {
        return Ok(InputData::Binary(body.to_vec()));
    }
    Ok(parse(body)
        .map(InputData::Json)
        .unwrap_or_else(|_| InputData::Binary(body.to_vec())))
}

enum Output {
    Json(Value),
    Binary { data: Vec<u8>, content_type: String },
}

/// Dispatch to the engine (a ready local model) or the provider gateway,
/// the same precedence as the gRPC service. Metrics are recorded here.
async fn run_predict(
    state: &ServerState,
    model: &str,
    input: InputData,
    headers: HeaderMap,
) -> Result<Output, ApiError> {
    let deadline = Instant::now() + state.settings.predict_timeout;
    let local = state.engine.is_model_loaded(model);
    let is_convert = if local {
        is_convert_model(state, model)
    } else {
        state.gateway.owns_converter(model)
    };
    let method = if is_convert {
        METHOD_CONVERT
    } else {
        METHOD_EMBED
    };
    let items = item_count(&input);
    if items > MAX_REQUEST_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "{items} items exceeds the {MAX_REQUEST_ITEMS} items-per-request ceiling; split the request"
        )));
    }

    let started = Instant::now();
    state.metrics.request_started(method);
    let result = if state.engine.is_model_ready(model) {
        engine_predict(state, model, input, headers, deadline).await
    } else if local {
        Err(ApiError::NotLoaded(format!(
            "model {model:?} is still loading on this node; retry shortly"
        )))
    } else if is_convert {
        gateway_convert(state, model, &input, deadline).await
    } else if state.gateway.owns(model) {
        gateway_embed(state, model, &input, deadline).await
    } else {
        Err(ApiError::NotLoaded(not_loaded(model)))
    };
    match &result {
        Ok(_) => state
            .metrics
            .request_completed(method, items, started.elapsed()),
        Err(e) => state.metrics.request_failed(method, e.metrics_label()),
    }
    result
}

fn is_convert_model(state: &ServerState, model: &str) -> bool {
    state
        .engine
        .get_model(model)
        .ok()
        .and_then(|m| {
            m.configuration()
                .params
                .get("model_type")
                .and_then(Value::as_str)
                .map(|t| t.eq_ignore_ascii_case("convert"))
        })
        .unwrap_or(false)
}

fn item_count(input: &InputData) -> usize {
    let first_array = |v: &Value| match v {
        Value::Object(map) => map.values().find_map(Value::as_array).map(Vec::len),
        Value::Array(items) => Some(items.len()),
        _ => None,
    };
    match input {
        InputData::Json(v) => first_array(v),
        InputData::Structured(values) => values.first().and_then(first_array),
        InputData::Binary(_) => None,
    }
    .unwrap_or(1)
}

async fn engine_predict(
    state: &ServerState,
    model: &str,
    input: InputData,
    headers: HeaderMap,
    deadline: Instant,
) -> Result<Output, ApiError> {
    let output = state
        .engine
        .clone()
        .predict_raw_at(model, input, headers, deadline)
        .await
        .map_err(ApiError::Engine)?;
    let keyed = |values: Vec<Value>| -> Result<Map<String, Value>, ApiError> {
        let loaded = state.engine.get_model(model).map_err(ApiError::Engine)?;
        let keys = loaded.configuration().executor.outputs.clone();
        Ok(keys.into_iter().map(|m| m.json_key).zip(values).collect())
    };
    Ok(match output {
        ExecutorOutput::Json(value) => Output::Json(value),
        ExecutorOutput::Binary { data, content_type } => Output::Binary { data, content_type },
        ExecutorOutput::Structured(values) => Output::Json(Value::Object(keyed(values)?)),
        ExecutorOutput::StructuredWithUsage { outputs, usage } => {
            let mut payload = keyed(outputs)?;
            payload.insert(
                "usage".into(),
                json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                }),
            );
            Output::Json(Value::Object(payload))
        }
    })
}

fn json_object<'a>(input: &'a InputData, what: &str) -> Result<&'a Map<String, Value>, ApiError> {
    match input {
        InputData::Json(Value::Object(map)) => Ok(map),
        _ => Err(ApiError::BadRequest(format!(
            "provider-backed models accept a JSON object with {what}"
        ))),
    }
}

async fn gateway_embed(
    state: &ServerState,
    model: &str,
    input: &InputData,
    deadline: Instant,
) -> Result<Output, ApiError> {
    let map = json_object(input, "a `texts` field")?;
    let texts = match map.get("texts") {
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        Some(Value::Array(items)) if !items.is_empty() && items.iter().all(Value::is_string) => {
            items
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        }
        None => return Err(ApiError::BadRequest("missing required key `texts`".into())),
        _ => {
            return Err(ApiError::BadRequest(
                "`texts` must be a non-empty string or array of strings".into(),
            ))
        }
    };
    // The gateway has no post-processing knobs; refusing beats silently
    // returning full-width floats to a caller that asked for something else.
    for (key, unsupported) in [("dimensions", true), ("encoding_format", false)] {
        let requested = match map.get(key) {
            None | Some(Value::Null) => false,
            Some(Value::String(s)) => !s.is_empty() && (unsupported || s != "float"),
            Some(Value::Number(n)) => unsupported && n.as_i64().unwrap_or(0) > 0,
            Some(_) => true,
        };
        if requested {
            return Err(ApiError::BadRequest(format!(
                "`{key}` is not supported for provider-backed model {model:?}"
            )));
        }
    }
    let input_type = map
        .get("input_type")
        .and_then(Value::as_str)
        .map(InputType::from_wire)
        .unwrap_or_default();
    let vectors = state
        .gateway
        .embed(model, &texts, input_type, deadline)
        .await
        .map_err(ApiError::Gateway)?;
    Ok(Output::Json(json!({ "embeddings": vectors })))
}

async fn gateway_convert(
    state: &ServerState,
    model: &str,
    input: &InputData,
    deadline: Instant,
) -> Result<Output, ApiError> {
    let map = json_object(input, "an `embeddings` field")?;
    let rows = map
        .get("embeddings")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ApiError::BadRequest("`embeddings` must be an array of number arrays".into())
        })?;
    let mut embeddings = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let cells = row.as_array().ok_or_else(|| {
            ApiError::BadRequest(format!("embeddings[{i}] must be an array of numbers"))
        })?;
        let vector = cells
            .iter()
            .map(|c| c.as_f64().map(|n| n as f32))
            .collect::<Option<Vec<f32>>>()
            .ok_or_else(|| {
                ApiError::BadRequest(format!("embeddings[{i}] contains a non-number"))
            })?;
        embeddings.push(vector);
    }
    let vectors = state
        .gateway
        .convert(model, &embeddings, deadline)
        .await
        .map_err(ApiError::Gateway)?;
    Ok(Output::Json(json!({ "embeddings": vectors })))
}

// ---- OpenAI adaptor ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct OpenAIEmbedPayload {
    model: String,
    #[serde(deserialize_with = "deserialize_input")]
    input: Vec<String>,
    #[serde(default)]
    encoding_format: Option<String>,
    #[serde(default)]
    dimensions: Option<u64>,
    #[serde(default)]
    input_type: Option<String>,
}

fn deserialize_input<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<String>, D::Error> {
    use serde::de::Error as _;
    let is_int_list = |v: &Value| {
        v.as_array()
            .map(|a| !a.is_empty() && a.iter().all(Value::is_number))
            .unwrap_or(false)
    };
    match Value::deserialize(de)? {
        Value::String(s) if !s.is_empty() => Ok(vec![s]),
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_string) => Ok(items
            .into_iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()),
        v if is_int_list(&v) || v.as_array().map(|a| !a.is_empty() && a.iter().all(is_int_list)).unwrap_or(false) => {
            Err(D::Error::custom(
                "`input` contains token IDs, which are not supported; provide a string or an array of strings",
            ))
        }
        _ => Err(D::Error::custom(
            "`input` must be a non-empty string or array of strings",
        )),
    }
}

/// A `postvec/` or `univec/` namespace is accepted and dropped.
fn model_name(raw: &str) -> &str {
    raw.strip_prefix("postvec/")
        .or_else(|| raw.strip_prefix("univec/"))
        .unwrap_or(raw)
}

/// Rewrite the OpenAI body into the native payload.
fn openai_to_native(payload: &OpenAIEmbedPayload, texts_key: &str) -> Value {
    let mut native = Map::new();
    native.insert(texts_key.into(), json!(payload.input));
    if let Some(fmt) = payload.encoding_format.as_deref().filter(|s| !s.is_empty()) {
        native.insert("encoding_format".into(), json!(fmt));
    }
    if let Some(dims) = payload.dimensions.filter(|d| *d > 0) {
        native.insert("dimensions".into(), json!(dims));
    }
    if let Some(t) = payload.input_type.as_deref().filter(|s| !s.is_empty()) {
        native.insert("input_type".into(), json!(t));
    }
    Value::Object(native)
}

/// `{embeddings, usage?}` (or whatever the model's first output is keyed)
/// into the OpenAI list envelope.
fn native_to_openai(model: &str, data: &Value) -> Result<Value, ApiError> {
    let rows = match data {
        Value::Array(items) => Some(items),
        Value::Object(map) => map
            .get("embeddings")
            .or_else(|| map.values().find(|v| v.is_array()))
            .and_then(Value::as_array),
        _ => None,
    }
    .ok_or_else(|| {
        ApiError::Internal("model response did not contain an embeddings array".into())
    })?;
    let data_rows: Vec<Value> = rows
        .iter()
        .enumerate()
        .map(|(index, embedding)| json!({ "object": "embedding", "index": index, "embedding": embedding }))
        .collect();
    let tokens = |k: &str| {
        data.pointer(&format!("/usage/{k}"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Ok(json!({
        "object": "list",
        "data": data_rows,
        "model": model,
        "usage": { "prompt_tokens": tokens("prompt_tokens"), "total_tokens": tokens("total_tokens") },
    }))
}

pub async fn openai_embeddings(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match openai_embeddings_inner(&state, headers, &body).await {
        Ok(value) => Json(value).into_response(),
        Err(e) => e.into_openai(),
    }
}

async fn openai_embeddings_inner(
    state: &ServerState,
    headers: HeaderMap,
    body: &[u8],
) -> Result<Value, ApiError> {
    let payload: OpenAIEmbedPayload = serde_json::from_slice(body).map_err(|e| {
        ApiError::BadRequest(match e.classify() {
            serde_json::error::Category::Data => e.to_string(),
            _ => format!("invalid JSON payload: {e}"),
        })
    })?;
    let model = model_name(&payload.model);
    let texts_key = state
        .engine
        .get_model(model)
        .ok()
        .and_then(|m| {
            m.configuration()
                .executor
                .inputs
                .first()
                .map(|i| i.json_key.clone())
        })
        .unwrap_or_else(|| "texts".into());
    let native = openai_to_native(&payload, &texts_key);
    if state.gateway.owns_converter(model) || is_convert_model(state, model) {
        return Err(ApiError::BadRequest(format!(
            "model {:?} is a converter; /api/openai/embeddings serves embed models only",
            payload.model
        )));
    }
    match run_predict(state, model, InputData::Json(native), headers).await? {
        Output::Json(data) => native_to_openai(&payload.model, &data),
        Output::Binary { .. } => Err(ApiError::Internal(
            "model returned a binary payload, not embeddings".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<OpenAIEmbedPayload, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[test]
    fn converter_resolution_prefers_local_and_matches_the_pair() {
        let models = postvec_core::client::discovery::parse_config(
            &json!({"success": true, "data": {"models": [
                {"name": "prov", "provider": "univec", "configuration": {"enabled": true, "params": {"model_type": "convert", "source_model": "a", "target_model": "b"}}},
                {"name": "local", "configuration": {"enabled": true, "params": {"model_type": "convert", "source_model": "a", "target_model": "b"}}},
                {"name": "other", "configuration": {"enabled": true, "params": {"model_type": "convert", "source_model": "a", "target_model": "c"}}},
                {"name": "emb", "configuration": {"enabled": true, "params": {"model_type": "embed", "target_model": "b"}}}
            ]}})
            .to_string(),
        )
        .unwrap();
        assert_eq!(converter_for(&models, "a", "b").as_deref(), Some("local"));
        assert_eq!(converter_for(&models, "a", "c").as_deref(), Some("other"));
        assert_eq!(converter_for(&models, "b", "a"), None);
    }

    #[test]
    fn input_accepts_a_string_or_a_string_list() {
        assert_eq!(
            parse(r#"{"model":"m","input":"hello"}"#).unwrap().input,
            ["hello"]
        );
        assert_eq!(
            parse(r#"{"model":"m","input":["a","b"]}"#).unwrap().input,
            ["a", "b"]
        );
    }

    #[test]
    fn input_rejects_token_ids_empties_and_mixed_lists() {
        let err = parse(r#"{"model":"m","input":[1,2,3]}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("token IDs"), "{err}");
        let err = parse(r#"{"model":"m","input":[[1,2],[3]]}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("token IDs"), "{err}");
        assert!(parse(r#"{"model":"m","input":[]}"#).is_err());
        assert!(parse(r#"{"model":"m","input":""}"#).is_err());
        assert!(parse(r#"{"model":"m","input":["a",1]}"#).is_err());
        assert!(parse(r#"{"model":"m","input":null}"#).is_err());
    }

    #[test]
    fn dimensions_is_an_optional_integer() {
        assert_eq!(
            parse(r#"{"model":"m","input":"x","dimensions":64}"#)
                .unwrap()
                .dimensions,
            Some(64)
        );
        assert_eq!(
            parse(r#"{"model":"m","input":"x","dimensions":null}"#)
                .unwrap()
                .dimensions,
            None
        );
        assert!(parse(r#"{"model":"m","input":"x","dimensions":"64"}"#).is_err());
        assert!(parse(r#"{"model":"m","input":"x","dimensions":-1}"#).is_err());
    }

    #[test]
    fn openai_to_native_rewrites_the_body_and_strips_the_namespace() {
        let payload = parse(
            r#"{"model":"postvec/mini","input":["hello","world"],"encoding_format":"float",
                "dimensions":64,"input_type":"search_query","user":"alice"}"#,
        )
        .unwrap();
        assert_eq!(model_name(&payload.model), "mini");
        assert_eq!(
            openai_to_native(&payload, "texts"),
            json!({"texts": ["hello", "world"], "encoding_format": "float", "dimensions": 64, "input_type": "search_query"})
        );

        let payload =
            parse(r#"{"model":"univec/bge","input":"x","dimensions":0,"encoding_format":""}"#)
                .unwrap();
        assert_eq!(model_name(&payload.model), "bge");
        assert_eq!(
            openai_to_native(&payload, "sentences"),
            json!({"sentences": ["x"]})
        );
    }

    #[test]
    fn native_to_openai_wraps_rows_and_echoes_the_model() {
        let data = json!({
            "embeddings": [[0.1, 0.2], [0.3, 0.4]],
            "usage": { "prompt_tokens": 7, "completion_tokens": 0, "total_tokens": 7 }
        });
        let out = native_to_openai("postvec/mini", &data).unwrap();
        assert_eq!(out["object"], json!("list"));
        assert_eq!(out["model"], json!("postvec/mini"));
        assert_eq!(
            out["data"][1],
            json!({"object": "embedding", "index": 1, "embedding": [0.3, 0.4]})
        );
        assert_eq!(out["usage"], json!({"prompt_tokens": 7, "total_tokens": 7}));

        let out = native_to_openai("m", &json!({"vectors": [[1.0]]})).unwrap();
        assert_eq!(out["data"].as_array().unwrap().len(), 1);
        assert_eq!(out["usage"]["prompt_tokens"], json!(0));
        assert!(native_to_openai("m", &json!({"score": 1})).is_err());
    }

    #[test]
    fn item_count_reads_the_first_array_field() {
        assert_eq!(
            item_count(&InputData::Json(json!({"texts": ["a", "b", "c"]}))),
            3
        );
        assert_eq!(
            item_count(&InputData::Json(
                json!({"dimensions": 4, "embeddings": [[1.0], [2.0]]})
            )),
            2
        );
        assert_eq!(item_count(&InputData::Json(json!({"texts": "a"}))), 1);
        assert_eq!(item_count(&InputData::Binary(vec![0])), 1);
    }

    #[test]
    fn detect_input_sniffs_json_without_a_content_type() {
        assert!(matches!(
            detect_input("", br#"{"a":1}"#),
            Ok(InputData::Json(_))
        ));
        assert!(matches!(
            detect_input("", b"\x89PNG"),
            Ok(InputData::Binary(_))
        ));
        assert!(matches!(
            detect_input("image/png", br#"{"a":1}"#),
            Ok(InputData::Binary(_))
        ));
        assert!(detect_input("application/json", b"{").is_err());
    }
}
