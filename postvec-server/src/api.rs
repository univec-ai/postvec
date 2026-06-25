//! Native HTTP inference (`/api/{model}`) and the OpenAI embeddings adaptor.
//!
//! The native contract is the same JSON the engine executors already speak:
//! a keyed object whose keys match `executor.inputs[].json_key` (typically
//! `texts` for embed models, `embeddings` for converters). The response is
//! the ninference envelope `{success, data}` / `{success, error:{message}}`
//! so a dashboard can treat HTTP 200 as "the server answered" and inspect
//! `success`.
//!
//! `/api/openai/embeddings` is a thin adaptor in front of that path: it
//! rewrites an OpenAI `/v1/embeddings` body into the native payload, runs
//! the same executor, and reshapes the result into `{object, data, model,
//! usage}`. It does not bypass the engine.

use crate::metrics::{Metrics, METHOD_CONVERT, METHOD_EMBED};
use crate::state::ServerState;
use axum::body::to_bytes;
use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use engine::{EngineError, ExecutorOutput, InferenceEngine, InputData, ModelOverview};
use providers::gateway::{GatewayError, InputType};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Instant;

/// Match the gRPC decode ceiling: a single HTTP body is not a way around it.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_JSON_AUTODETECT: usize = 1_048_576;

/// Native success/error envelope. HTTP stays 200 except for routing
/// mistakes, matching the ninference dashboard contract.
fn native_ok(data: Value) -> Response {
    Json(json!({ "success": true, "data": data })).into_response()
}

fn native_err(message: impl Into<String>) -> Response {
    Json(json!({
        "success": false,
        "error": { "message": message.into() }
    }))
    .into_response()
}

fn openai_err(status: StatusCode, message: impl Into<String>, kind: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message.into(),
                "type": kind,
                "code": Value::Null,
                "param": Value::Null,
            }
        })),
    )
        .into_response()
}

/// `{success:false}` for unmatched `/api/*` paths, as JSON rather than
/// Axum's HTML 404.
pub async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "success": false,
            "error": { "message": "the requested API endpoint does not exist" }
        })),
    )
        .into_response()
}

pub async fn model_details(
    State(state): State<Arc<ServerState>>,
    Path(model_name): Path<String>,
) -> Response {
    if let Ok(model) = state.engine.get_model(&model_name) {
        match model.overview() {
            Ok(overview) => native_ok(overview_json(&overview)),
            Err(e) => native_err(e.to_string()),
        }
    } else if state.gateway.owns(&model_name) || state.gateway.owns_converter(&model_name) {
        native_ok(json!({
            "inputs": [],
            "outputs": [],
            "provider": true,
        }))
    } else {
        native_err(format!(
            "model {model_name:?} is not loaded on this node; put it on disk \
             (`postvec model pull {model_name}`) and load it with \
             `postvec-server load {model_name}` — inference requests never load models"
        ))
    }
}

fn overview_json(overview: &ModelOverview) -> Value {
    serde_json::to_value(overview).unwrap_or(json!({"inputs": [], "outputs": []}))
}

pub async fn predict(
    State(state): State<Arc<ServerState>>,
    Path(model_name): Path<String>,
    req: Request,
) -> Response {
    match predict_native(&state, &model_name, req).await {
        Ok(response) => response,
        Err(err) => err.into_native(),
    }
}

pub async fn openai_embeddings(State(state): State<Arc<ServerState>>, req: Request) -> Response {
    match openai_embeddings_inner(&state, req).await {
        Ok(response) => response,
        Err(err) => err.into_openai(),
    }
}

#[derive(Debug)]
enum ApiError {
    BadRequest(String),
    #[allow(dead_code)]
    NotFound(String),
    NotLoaded(String),
    #[allow(dead_code)]
    Timeout(String),
    Engine(EngineError),
    Gateway(GatewayError),
    Internal(String),
}

impl ApiError {
    fn message(&self) -> String {
        match self {
            ApiError::BadRequest(m)
            | ApiError::NotFound(m)
            | ApiError::NotLoaded(m)
            | ApiError::Timeout(m)
            | ApiError::Internal(m) => m.clone(),
            ApiError::Engine(e) => e.to_string(),
            ApiError::Gateway(e) => e.message.clone(),
        }
    }

    fn into_native(self) -> Response {
        native_err(self.message())
    }

    fn into_openai(self) -> Response {
        match self {
            ApiError::BadRequest(m) => {
                openai_err(StatusCode::BAD_REQUEST, m, "invalid_request_error")
            }
            ApiError::NotFound(m) | ApiError::NotLoaded(m) => {
                openai_err(StatusCode::NOT_FOUND, m, "not_found_error")
            }
            ApiError::Timeout(m) => openai_err(StatusCode::GATEWAY_TIMEOUT, m, "timeout_error"),
            ApiError::Engine(e) => openai_from_engine(e),
            ApiError::Gateway(e) => openai_from_gateway(e),
            ApiError::Internal(m) => openai_err(StatusCode::INTERNAL_SERVER_ERROR, m, "api_error"),
        }
    }
}

fn openai_from_engine(e: EngineError) -> Response {
    let code = e.to_error_code();
    let message = e.to_string();
    let (status, kind) = match code {
        shared::ErrorCode::InvalidInput | shared::ErrorCode::ContextLengthExceeded => {
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        }
        shared::ErrorCode::ModelNotFound | shared::ErrorCode::ModelNotLoaded => {
            (StatusCode::NOT_FOUND, "not_found_error")
        }
        shared::ErrorCode::Timeout => (StatusCode::GATEWAY_TIMEOUT, "timeout_error"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
    };
    openai_err(status, message, kind)
}

fn openai_from_gateway(e: GatewayError) -> Response {
    let (status, kind) = match e.code {
        shared::ErrorCode::InvalidInput | shared::ErrorCode::ContextLengthExceeded => {
            (StatusCode::BAD_REQUEST, "invalid_request_error")
        }
        shared::ErrorCode::ModelNotFound => (StatusCode::NOT_FOUND, "not_found_error"),
        shared::ErrorCode::Timeout => (StatusCode::GATEWAY_TIMEOUT, "timeout_error"),
        shared::ErrorCode::UpstreamAuthFailed => (StatusCode::UNAUTHORIZED, "authentication_error"),
        shared::ErrorCode::UpstreamServiceUnavailable => (StatusCode::BAD_GATEWAY, "api_error"),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "api_error"),
    };
    openai_err(status, e.message, kind)
}

async fn predict_native(
    state: &ServerState,
    model_name: &str,
    req: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let headers = parts.headers;
    let body_bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|e| ApiError::BadRequest(format!("failed to read request body: {e}")))?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let input = detect_input(content_type, &body_bytes)?;
    run_predict(state, model_name, input, headers).await
}

fn detect_input(content_type: &str, body: &[u8]) -> Result<InputData, ApiError> {
    if content_type.starts_with("application/json") {
        let payload: Value = serde_json::from_slice(body)
            .map_err(|e| ApiError::BadRequest(format!("invalid JSON payload: {e}")))?;
        return Ok(InputData::Json(payload));
    }
    if is_binary_content_type(content_type) {
        return Ok(InputData::Binary(body.to_vec()));
    }
    if body.len() <= MAX_JSON_AUTODETECT {
        match serde_json::from_slice::<Value>(body) {
            Ok(payload) => Ok(InputData::Json(payload)),
            Err(_) => Ok(InputData::Binary(body.to_vec())),
        }
    } else {
        Ok(InputData::Binary(body.to_vec()))
    }
}

fn is_binary_content_type(content_type: &str) -> bool {
    content_type.starts_with("image/")
        || content_type.starts_with("audio/")
        || content_type.starts_with("video/")
        || content_type == "application/octet-stream"
}

async fn run_predict(
    state: &ServerState,
    model_name: &str,
    input: InputData,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let deadline = Instant::now() + state.settings.predict_timeout;
    if state.engine.is_model_ready(model_name) {
        return engine_predict(state, model_name, input, headers, deadline).await;
    }
    if !state.engine.is_model_loaded(model_name) && state.gateway.owns(model_name) {
        return gateway_embed(state, model_name, &input, deadline).await;
    }
    if !state.engine.is_model_loaded(model_name) && state.gateway.owns_converter(model_name) {
        return gateway_convert(state, model_name, &input, deadline).await;
    }
    Err(ApiError::NotLoaded(format!(
        "model {model_name:?} is not loaded on this node; put it on disk \
         (`postvec model pull {model_name}`) and load it with \
         `postvec-server load {model_name}` — inference requests never load models"
    )))
}

async fn engine_predict(
    state: &ServerState,
    model_name: &str,
    input: InputData,
    headers: HeaderMap,
    deadline: Instant,
) -> Result<Response, ApiError> {
    let method = method_for_engine(&state.engine, model_name);
    let items = item_count(&input);
    let started = Instant::now();
    state.metrics.request_started(method);
    let result = state
        .engine
        .clone()
        .predict_raw_at(model_name, input, headers, deadline)
        .await;
    match result {
        Ok(output) => {
            state
                .metrics
                .request_completed(method, items, started.elapsed());
            Ok(executor_output_to_response(
                &state.engine,
                model_name,
                output,
            )?)
        }
        Err(e) => {
            state
                .metrics
                .request_failed(method, e.to_error_code().as_str());
            Err(ApiError::Engine(e))
        }
    }
}

fn method_for_engine(engine: &InferenceEngine, model_name: &str) -> usize {
    engine
        .get_model(model_name)
        .ok()
        .and_then(|m| {
            m.configuration()
                .params
                .get("model_type")
                .and_then(|v| v.as_str())
                .map(|t| {
                    if t.eq_ignore_ascii_case("convert") {
                        METHOD_CONVERT
                    } else {
                        METHOD_EMBED
                    }
                })
        })
        .unwrap_or(METHOD_EMBED)
}

fn item_count(input: &InputData) -> usize {
    match input {
        InputData::Json(Value::Object(map)) => map
            .get("texts")
            .and_then(Value::as_array)
            .or_else(|| map.get("embeddings").and_then(Value::as_array))
            .map(Vec::len)
            .unwrap_or(1),
        InputData::Structured(values) => values
            .first()
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(1),
        _ => 1,
    }
}

fn executor_output_to_response(
    engine: &InferenceEngine,
    model_name: &str,
    output: ExecutorOutput,
) -> Result<Response, ApiError> {
    match output {
        ExecutorOutput::Json(value) => Ok(native_ok(value)),
        ExecutorOutput::Binary { data, content_type } => {
            let parsed = content_type
                .parse()
                .unwrap_or_else(|_| header::HeaderValue::from_static("application/octet-stream"));
            Ok(([(header::CONTENT_TYPE, parsed)], data).into_response())
        }
        ExecutorOutput::Structured(values) => Ok(native_ok(structured_payload(
            engine, model_name, values, None,
        )?)),
        ExecutorOutput::StructuredWithUsage { outputs, usage } => {
            let mut payload = structured_payload(engine, model_name, outputs, None)?
                .as_object()
                .cloned()
                .unwrap_or_default();
            payload.insert(
                "usage".to_string(),
                json!({
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                }),
            );
            Ok(native_ok(Value::Object(payload)))
        }
    }
}

fn structured_payload(
    engine: &InferenceEngine,
    model_name: &str,
    values: Vec<Value>,
    extra: Option<(&str, Value)>,
) -> Result<Value, ApiError> {
    let model = engine.get_model(model_name).map_err(ApiError::Engine)?;
    let mappings = &model.configuration().executor.outputs;
    let mut payload = Map::new();
    for (mapping, value) in mappings.iter().zip(values) {
        payload.insert(mapping.json_key.clone(), value);
    }
    if let Some((k, v)) = extra {
        payload.insert(k.to_string(), v);
    }
    Ok(Value::Object(payload))
}

async fn gateway_embed(
    state: &ServerState,
    model_name: &str,
    input: &InputData,
    deadline: Instant,
) -> Result<Response, ApiError> {
    let texts = texts_from_input(input)?;
    if texts.len() > crate::limits::MAX_REQUEST_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "{} texts exceeds the {} items-per-request ceiling; split the request",
            texts.len(),
            crate::limits::MAX_REQUEST_ITEMS
        )));
    }
    let input_type = input_type_from_input(input);
    instrument_gateway(&state.metrics, METHOD_EMBED, texts.len(), async {
        state
            .gateway
            .embed(model_name, &texts, input_type, deadline)
            .await
            .map_err(ApiError::Gateway)
    })
    .await
    .map(|vectors| native_ok(json!({ "embeddings": vectors })))
}

async fn gateway_convert(
    state: &ServerState,
    model_name: &str,
    input: &InputData,
    deadline: Instant,
) -> Result<Response, ApiError> {
    let embeddings = embeddings_from_input(input)?;
    if embeddings.len() > crate::limits::MAX_REQUEST_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "{} embeddings exceeds the {} items-per-request ceiling; split the request",
            embeddings.len(),
            crate::limits::MAX_REQUEST_ITEMS
        )));
    }
    instrument_gateway(&state.metrics, METHOD_CONVERT, embeddings.len(), async {
        state
            .gateway
            .convert(model_name, &embeddings, deadline)
            .await
            .map_err(ApiError::Gateway)
    })
    .await
    .map(|vectors| native_ok(json!({ "embeddings": vectors })))
}

async fn instrument_gateway<F, T>(
    metrics: &Metrics,
    method: usize,
    items: usize,
    fut: F,
) -> Result<T, ApiError>
where
    F: std::future::Future<Output = Result<T, ApiError>>,
{
    let started = Instant::now();
    metrics.request_started(method);
    match fut.await {
        Ok(value) => {
            metrics.request_completed(method, items, started.elapsed());
            Ok(value)
        }
        Err(err) => {
            let label = match &err {
                ApiError::Gateway(e) => e.code.as_str(),
                _ => "OTHER",
            };
            metrics.request_failed(method, label);
            Err(err)
        }
    }
}

fn texts_from_input(input: &InputData) -> Result<Vec<String>, ApiError> {
    let payload = match input {
        InputData::Json(v) => v,
        _ => {
            return Err(ApiError::BadRequest(
                "provider-backed embed models accept a JSON body with a `texts` field".into(),
            ))
        }
    };
    parse_string_list(payload.get("texts").unwrap_or(&Value::Null), "texts")
}

fn embeddings_from_input(input: &InputData) -> Result<Vec<Vec<f32>>, ApiError> {
    let payload = match input {
        InputData::Json(v) => v,
        InputData::Structured(values) => values
            .first()
            .ok_or_else(|| ApiError::BadRequest("conversion payload is empty".into()))?,
        _ => {
            return Err(ApiError::BadRequest(
                "provider-backed converters accept a JSON body with an `embeddings` field".into(),
            ))
        }
    };
    let value = match payload {
        Value::Object(map) => map
            .get("embeddings")
            .ok_or_else(|| ApiError::BadRequest("missing required key `embeddings`".into()))?,
        Value::Array(_) => payload,
        _ => {
            return Err(ApiError::BadRequest(
                "`embeddings` must be an array of number arrays".into(),
            ))
        }
    };
    let rows = value.as_array().ok_or_else(|| {
        ApiError::BadRequest("`embeddings` must be an array of number arrays".into())
    })?;
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.iter().enumerate() {
        let cells = row.as_array().ok_or_else(|| {
            ApiError::BadRequest(format!("embeddings[{i}] must be an array of numbers"))
        })?;
        let mut vec = Vec::with_capacity(cells.len());
        for (j, cell) in cells.iter().enumerate() {
            let n = cell.as_f64().ok_or_else(|| {
                ApiError::BadRequest(format!("embeddings[{i}][{j}] is not a number"))
            })?;
            vec.push(n as f32);
        }
        out.push(vec);
    }
    Ok(out)
}

fn input_type_from_input(input: &InputData) -> InputType {
    match input {
        InputData::Json(Value::Object(map)) => map
            .get("input_type")
            .and_then(Value::as_str)
            .map(InputType::from_wire)
            .unwrap_or_default(),
        _ => InputType::default(),
    }
}

fn parse_string_list(value: &Value, field: &str) -> Result<Vec<String>, ApiError> {
    match value {
        Value::String(s) => {
            if s.is_empty() {
                Err(ApiError::BadRequest(format!(
                    "`{field}` must be a non-empty string or array of strings"
                )))
            } else {
                Ok(vec![s.clone()])
            }
        }
        Value::Array(items) if items.is_empty() => Err(ApiError::BadRequest(format!(
            "`{field}` must be a non-empty string or array of strings"
        ))),
        Value::Array(items) if items.iter().all(Value::is_string) => Ok(items
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()),
        Value::Null => Err(ApiError::BadRequest(format!(
            "missing required key `{field}`"
        ))),
        _ => Err(ApiError::BadRequest(format!(
            "`{field}` must be a string or an array of strings"
        ))),
    }
}

// ---- OpenAI adaptor ----------------------------------------------------

#[derive(Debug, Deserialize)]
struct OpenAIEmbedPayload {
    model: String,
    #[serde(deserialize_with = "deserialize_string_or_string_list")]
    input: Vec<String>,
    #[serde(default)]
    encoding_format: Option<String>,
    #[serde(default, deserialize_with = "deserialize_lenient_dimensions")]
    dimensions: u32,
    #[serde(default)]
    input_type: Option<String>,
    #[serde(default)]
    user: Option<String>,
}

fn deserialize_string_or_string_list<'de, D>(de: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let v = Value::deserialize(de)?;
    match v {
        Value::String(s) => Ok(vec![s]),
        Value::Array(items) => {
            if items.is_empty() {
                return Err(D::Error::custom(
                    "`input` must be a non-empty string or array of strings",
                ));
            }
            if items.iter().all(Value::is_string) {
                return Ok(items
                    .into_iter()
                    .map(|x| x.as_str().unwrap().to_string())
                    .collect());
            }
            if items.iter().all(Value::is_number)
                || items.iter().all(|x| {
                    x.as_array()
                        .map(|inner| inner.iter().all(Value::is_number))
                        .unwrap_or(false)
                })
            {
                return Err(D::Error::custom(
                    "`input` contains token IDs, which are not supported. Provide a string or an array of strings instead.",
                ));
            }
            Err(D::Error::custom(
                "`input` must be a string or an array of strings; mixed or nested types are not supported",
            ))
        }
        Value::Null => Err(D::Error::custom("`input` must not be null")),
        _ => Err(D::Error::custom(
            "`input` must be a string or an array of strings",
        )),
    }
}

fn deserialize_lenient_dimensions<'de, D>(de: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error as _;
    let clamp = |v: i64| -> u32 {
        if v <= 0 {
            0
        } else {
            v.min(u32::MAX as i64) as u32
        }
    };
    match Value::deserialize(de)? {
        Value::Null => Ok(0),
        Value::Number(n) => {
            if let Some(v) = n.as_i64() {
                Ok(clamp(v))
            } else if let Some(v) = n.as_u64() {
                Ok(v.min(u32::MAX as u64) as u32)
            } else {
                Err(D::Error::custom(format!(
                    "`dimensions` must be a positive integer, got {n}"
                )))
            }
        }
        Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Ok(0)
            } else {
                trimmed.parse::<i64>().map(clamp).map_err(|_| {
                    D::Error::custom(format!(
                        "`dimensions` must be a positive integer, got '{s}'"
                    ))
                })
            }
        }
        other => Err(D::Error::custom(format!(
            "`dimensions` must be a positive integer, got {other}"
        ))),
    }
}

/// Rewrite an OpenAI embeddings body into the native executor payload.
///
/// The engine's `transformer-sequence-embedding` reads `texts` (required)
/// plus optional `encoding_format` / `dimensions` / `input_type`. The
/// OpenAI `model` field is the path; `user` is not forwarded (the executor
/// has no mapping for it).
fn openai_to_native(payload: &OpenAIEmbedPayload) -> (String, Value) {
    let model = payload
        .model
        .strip_prefix("postvec/")
        .or_else(|| payload.model.strip_prefix("univec/"))
        .unwrap_or(&payload.model)
        .to_string();
    let mut native = Map::new();
    native.insert(
        "texts".to_string(),
        Value::Array(payload.input.iter().cloned().map(Value::String).collect()),
    );
    if let Some(fmt) = &payload.encoding_format {
        if !fmt.is_empty() {
            native.insert("encoding_format".to_string(), Value::String(fmt.clone()));
        }
    }
    if payload.dimensions > 0 {
        native.insert(
            "dimensions".to_string(),
            Value::Number(payload.dimensions.into()),
        );
    }
    if let Some(input_type) = &payload.input_type {
        if !input_type.is_empty() {
            native.insert("input_type".to_string(), Value::String(input_type.clone()));
        }
    }
    let _ = &payload.user;
    (model, Value::Object(native))
}

fn native_data_to_openai(model: &str, data: &Value) -> Result<Value, ApiError> {
    let embeddings = match data {
        Value::Array(items) => items.clone(),
        Value::Object(map) => map
            .get("embeddings")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| {
                ApiError::Internal("native response did not contain an `embeddings` array".into())
            })?,
        _ => {
            return Err(ApiError::Internal(
                "native response was not an embeddings payload".into(),
            ))
        }
    };
    let data_rows: Vec<Value> = embeddings
        .into_iter()
        .enumerate()
        .map(|(index, embedding)| {
            json!({
                "object": "embedding",
                "index": index,
                "embedding": embedding,
            })
        })
        .collect();
    let usage = match data.get("usage") {
        Some(Value::Object(u)) => json!({
            "prompt_tokens": u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0),
            "total_tokens": u.get("total_tokens").and_then(Value::as_u64).unwrap_or(0),
        }),
        _ => json!({ "prompt_tokens": 0, "total_tokens": 0 }),
    };
    Ok(json!({
        "object": "list",
        "data": data_rows,
        "model": model,
        "usage": usage,
    }))
}

async fn openai_embeddings_inner(state: &ServerState, req: Request) -> Result<Response, ApiError> {
    let body_bytes = to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|e| ApiError::BadRequest(format!("failed to read request body: {e}")))?;
    let payload: OpenAIEmbedPayload = serde_json::from_slice(&body_bytes)
        .map_err(|e| ApiError::BadRequest(format!("invalid JSON payload: {e}")))?;
    if payload.input.is_empty() {
        return Err(ApiError::BadRequest(
            "`input` must be a non-empty string or array of strings".into(),
        ));
    }
    if payload.input.len() > crate::limits::MAX_REQUEST_ITEMS {
        return Err(ApiError::BadRequest(format!(
            "{} inputs exceeds the {} items-per-request ceiling; split the request",
            payload.input.len(),
            crate::limits::MAX_REQUEST_ITEMS
        )));
    }
    let echoed_model = payload.model.clone();
    let (model, native) = openai_to_native(&payload);
    if state.gateway.owns_converter(&model) || is_convert_engine_model(&state.engine, &model) {
        return Err(ApiError::BadRequest(format!(
            "model {echoed_model:?} is a converter; /api/openai/embeddings serves embed models only"
        )));
    }
    let response = run_predict(state, &model, InputData::Json(native), HeaderMap::new()).await?;
    // Native success is HTTP 200 `{success:true, data}` — peel it back so
    // the OpenAI caller never sees the internal envelope.
    let (parts, body) = response.into_parts();
    if parts.status != StatusCode::OK {
        return Ok(Response::from_parts(parts, body));
    }
    let bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|e| ApiError::Internal(format!("failed to read native response: {e}")))?;
    let envelope: Value = serde_json::from_slice(&bytes)
        .map_err(|e| ApiError::Internal(format!("native response was not JSON: {e}")))?;
    if envelope.get("success") == Some(&Value::Bool(false)) {
        let message = envelope
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("inference failed")
            .to_string();
        return Err(ApiError::Internal(message));
    }
    let data = envelope.get("data").cloned().unwrap_or(Value::Null);
    let openai = native_data_to_openai(&echoed_model, &data)?;
    Ok(Json(openai).into_response())
}

fn is_convert_engine_model(engine: &InferenceEngine, model: &str) -> bool {
    engine
        .get_model(model)
        .ok()
        .and_then(|m| {
            m.configuration()
                .params
                .get("model_type")
                .and_then(|v| v.as_str())
                .map(|t| t.eq_ignore_ascii_case("convert"))
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct InputWrap {
        #[serde(deserialize_with = "deserialize_string_or_string_list")]
        input: Vec<String>,
    }

    fn parse_input(json: &str) -> Result<Vec<String>, serde_json::Error> {
        serde_json::from_str::<InputWrap>(json).map(|w| w.input)
    }

    #[test]
    fn input_accepts_single_string() {
        assert_eq!(parse_input(r#"{"input":"hello"}"#).unwrap(), vec!["hello"]);
    }

    #[test]
    fn input_accepts_array_of_strings() {
        assert_eq!(
            parse_input(r#"{"input":["a","b"]}"#).unwrap(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn input_rejects_token_ids() {
        let err = parse_input(r#"{"input":[1,2,3]}"#).unwrap_err().to_string();
        assert!(err.contains("token IDs"), "{err}");
    }

    #[test]
    fn input_rejects_empty_array() {
        assert!(parse_input(r#"{"input":[]}"#).is_err());
    }

    #[test]
    fn openai_to_native_rewrites_input_to_texts_and_strips_prefix() {
        let payload = OpenAIEmbedPayload {
            model: "postvec/mini".into(),
            input: vec!["hello".into(), "world".into()],
            encoding_format: Some("float".into()),
            dimensions: 64,
            input_type: Some("search_query".into()),
            user: Some("alice".into()),
        };
        let (model, native) = openai_to_native(&payload);
        assert_eq!(model, "mini");
        assert_eq!(native["texts"], json!(["hello", "world"]));
        assert_eq!(native["encoding_format"], json!("float"));
        assert_eq!(native["dimensions"], json!(64));
        assert_eq!(native["input_type"], json!("search_query"));
        assert!(
            native.get("user").is_none(),
            "user is not an executor input"
        );
        assert!(
            native.get("model").is_none(),
            "model is the path, not a field"
        );
    }

    #[test]
    fn openai_to_native_also_strips_univec_prefix() {
        let payload = OpenAIEmbedPayload {
            model: "univec/baai-bge-m3".into(),
            input: vec!["x".into()],
            encoding_format: None,
            dimensions: 0,
            input_type: None,
            user: None,
        };
        let (model, native) = openai_to_native(&payload);
        assert_eq!(model, "baai-bge-m3");
        assert!(native.get("dimensions").is_none());
        assert!(native.get("encoding_format").is_none());
    }

    #[test]
    fn native_data_to_openai_wraps_rows_and_echoes_the_model() {
        let data = json!({
            "embeddings": [[0.1, 0.2], [0.3, 0.4]],
            "usage": { "prompt_tokens": 7, "completion_tokens": 0, "total_tokens": 7 }
        });
        let out = native_data_to_openai("postvec/mini", &data).unwrap();
        assert_eq!(out["object"], json!("list"));
        assert_eq!(out["model"], json!("postvec/mini"));
        assert_eq!(out["data"][0]["index"], json!(0));
        assert_eq!(out["data"][0]["embedding"], json!([0.1, 0.2]));
        assert_eq!(out["data"][1]["index"], json!(1));
        assert_eq!(out["usage"]["prompt_tokens"], json!(7));
        assert_eq!(out["usage"]["total_tokens"], json!(7));
    }

    #[test]
    fn native_data_to_openai_accepts_a_bare_array() {
        let out = native_data_to_openai("m", &json!([[1.0]])).unwrap();
        assert_eq!(out["data"].as_array().unwrap().len(), 1);
        assert_eq!(out["usage"]["prompt_tokens"], json!(0));
    }

    #[test]
    fn lenient_dimensions_treats_blank_as_full_vector() {
        #[derive(Deserialize)]
        struct Wrap {
            #[serde(default, deserialize_with = "deserialize_lenient_dimensions")]
            dimensions: u32,
        }
        let zero = |s: &str| serde_json::from_str::<Wrap>(s).unwrap().dimensions;
        assert_eq!(zero(r#"{"dimensions":null}"#), 0);
        assert_eq!(zero(r#"{"dimensions":0}"#), 0);
        assert_eq!(zero(r#"{"dimensions":-1}"#), 0);
        assert_eq!(zero(r#"{"dimensions":""}"#), 0);
        assert_eq!(zero(r#"{"dimensions":"32"}"#), 32);
        assert_eq!(zero(r#"{"dimensions":32}"#), 32);
    }
}
