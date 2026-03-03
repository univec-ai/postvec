//! The in-worker loopback gRPC server (embedded mode).
//!
//! Serves the vendored ninference proto so connection backends can keep
//! using the unchanged `GrpcClient` for `search()`/`embed()`/`convert()`.
//! Same shape as remote mode, just with the server hosted inside the
//! bgworker. The handler bodies and error taxonomy are a port of
//! the production ninference gRPC server (minus metrics), so behavior matches a
//! remote ninference node byte for byte: same payload shapes into
//! `predict_raw`, same `x-ravenna-error-code` metadata out.
//!
//! Runs entirely on the engine runtime. Nothing in this module may touch
//! Postgres (no SPI, no pgrx `elog`); logging goes through the `log` crate,
//! which embedded mode bridges to stderr.

use crate::proto::ninference_service_server::{NinferenceService, NinferenceServiceServer};
use crate::proto::{
    ConvertEmbeddingsRequest, ConvertEmbeddingsResponse, EmbedTextsRequest, EmbedTextsResponse,
    TokenUsage,
};
use engine::{EngineError, ExecutorOutput, InferenceEngine, InputData};
use prost_types::{ListValue, Value as ProstValue};
use providers::gateway::{Gateway, GatewayError, InputType};
use serde_json::Value;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::codegen::Body;
use tonic::{Request, Response, Status};
use tower::{Layer, Service};

/// Mirror of the client-side envelope (`client/grpc.rs`): decode (requests
/// arriving here) is bounded at the client's encode ceiling, and encode
/// (responses leaving here) at the client's decode ceiling. The old 256 MiB
/// blanket invited GB-scale decode amplification in the engine host — the
/// process whose death restarts the whole cluster.
const MAX_DECODE_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
const MAX_ENCODE_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

/// The inbound gRPC deadline from the `grpc-timeout` request header (tonic
/// surfaces it as metadata): 1-8 ASCII digits plus a unit. The loopback
/// server must honor it — `predict_timeout` (postvec.embed_timeout_ms,
/// default 30 s) is a CEILING for callers that send no deadline, not a grant
/// that outlives the caller: a 2 s `search()` must not leave 30 s of native
/// work running (and holding the admission permit) after the SQL caller has
/// given up.
fn inbound_deadline(md: &tonic::metadata::MetadataMap) -> Option<Duration> {
    let raw = md.get("grpc-timeout")?.to_str().ok()?;
    parse_grpc_timeout(raw)
}

fn parse_grpc_timeout(raw: &str) -> Option<Duration> {
    if raw.len() < 2 {
        return None;
    }
    let (digits, unit) = raw.split_at(raw.len() - 1);
    // gRPC's wire grammar permits at most eight decimal digits. Enforcing it
    // also prevents a hostile saturating duration from reaching Instant
    // arithmetic.
    if digits.is_empty() || digits.len() > 8 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u64 = digits.parse().ok()?;
    Some(match unit {
        "H" => Duration::from_secs(v.saturating_mul(3600)),
        "M" => Duration::from_secs(v.saturating_mul(60)),
        "S" => Duration::from_secs(v),
        "m" => Duration::from_millis(v),
        "u" => Duration::from_micros(v),
        "n" => Duration::from_nanos(v),
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug)]
struct AbsoluteDeadline(std::time::Instant);

fn deadline_from_budget(now: std::time::Instant, budget: Duration) -> std::time::Instant {
    // `budget` is clamped to the configured server ceiling before this call,
    // but keep arithmetic total if that ceiling is ever made operator-sized.
    now.checked_add(budget).unwrap_or(now)
}

fn request_deadline<T>(request: &Request<T>, ceiling: Duration) -> std::time::Instant {
    if let Some(deadline) = request.extensions().get::<AbsoluteDeadline>() {
        return deadline.0;
    }
    // Direct unit calls do not pass through the HTTP layer below.
    let budget = inbound_deadline(request.metadata()).map_or(ceiling, |d| d.min(ceiling));
    deadline_from_budget(std::time::Instant::now(), budget)
}

/// Ceiling on the transient input/output trees one loopback request may make
/// the launcher build. Input accounting charges the decoded f32 plus JSON
/// `Value`; output accounting charges the coexisting JSON plus prost values,
/// using their actual Rust sizes below and adding per-row overhead. 96 MiB
/// keeps tree amplification independently below the wire ceiling;
/// postvec's own client uses the same conservative budget.
const OUTPUT_TREE_BUDGET_BYTES: u64 = 96 * 1024 * 1024;

/// Estimated per-item container overhead in a decoded request + JSON tree:
/// a prost message + `Vec` header on the wire side and a
/// `Value::Array(Vec<Value>)` row on the JSON side. Deliberately generous.
const TREE_ITEM_OVERHEAD_BYTES: u64 = 256;
const JSON_COMPONENT_BYTES: u64 = std::mem::size_of::<Value>() as u64;
const PROST_COMPONENT_BYTES: u64 = std::mem::size_of::<ProstValue>() as u64;
const INPUT_COMPONENT_TRANSIENT_BYTES: u64 =
    JSON_COMPONENT_BYTES + std::mem::size_of::<f32>() as u64;
const OUTPUT_COMPONENT_TRANSIENT_BYTES: u64 = JSON_COMPONENT_BYTES + PROST_COMPONENT_BYTES;
const DEADLINE_CHECK_COMPONENTS: u64 = 65_536;

/// Upper bound on a *resolved* response dimension. No real embedding model
/// is within two orders of magnitude of this; a larger value in a model's
/// configuration is corrupt or hostile, and casting it onward (i32) or
/// sizing the response guard from it would unbound the envelope — refuse
/// instead of falling back.
const MAX_PLAUSIBLE_DIM: i64 = 100_000;

/// The response dimension for a request: the model's own `target_dim`, or
/// the bridge chain's converter (resolved exactly as the executor will).
/// `Ok(None)` = genuinely unknown — callers fall back to the conservative
/// 16,000-dim guard. A resolved value above [`MAX_PLAUSIBLE_DIM`] is a
/// refusal, never a fallback.
#[allow(clippy::result_large_err)]
fn resolved_response_dim(
    engine: &InferenceEngine,
    model: &str,
    bridge_model: &str,
    target_model: &str,
) -> Result<Option<i32>, Status> {
    let mut dim = engine
        .get_model(model)
        .map(|m| m.param_int_or_default("target_dim", 0))
        .unwrap_or(0);
    if dim <= 0 && !bridge_model.is_empty() && !target_model.is_empty() {
        if let Some(resolved) = engine.resolver.resolve_convert(bridge_model, target_model) {
            dim = engine
                .get_model(&resolved.internal_name)
                .map(|m| m.param_int_or_default("target_dim", 0))
                .unwrap_or(0);
        }
    }
    if dim <= 0 {
        return Ok(None);
    }
    if dim > MAX_PLAUSIBLE_DIM {
        return Err(invalid_input_status(format!(
            "model {model:?} reports an implausible target dimension {dim}; refusing to \
             size a response from it"
        )));
    }
    Ok(Some(dim as i32))
}

struct EmbeddedService {
    engine: Arc<InferenceEngine>,
    predict_timeout: Duration,
    /// A completed unary response is encoded lazily after the handler future
    /// returns. This semaphore's permit is moved into the HTTP response body,
    /// so slow/abandoned loopback readers cannot accumulate unbounded prost
    /// response trees after Tower's request-future limit has been released.
    /// Engine path only — the provider path never takes a slot (§7.3, see
    /// the dispatch comment in `embed_texts`).
    response_slots: Arc<Semaphore>,
    /// The same mechanism for the provider path, on its own budget.
    ///
    /// The provider path deliberately skips `response_slots` so a network
    /// call never queues behind CPU-bound ONNX — but "no admission gate" and
    /// "no *response-lifetime* bound" are different things, and it had
    /// neither. A completed unary response is encoded lazily after the
    /// handler returns, so the per-provider semaphore and the tower ingress
    /// permit are both released while the prost tree is still resident. A
    /// slow or abandoned reader could accumulate them without limit.
    ///
    /// Sized at the gateway's boot-time inflight budget — the sum of the
    /// per-provider `max_concurrent` caps — so it can never be the binding
    /// constraint on *admission* (those semaphores already are), and only
    /// bites when responses linger, which is exactly the condition to bound.
    provider_response_slots: Arc<Semaphore>,
    /// External-provider gateway (docs/external-providers.md). Empty in the
    /// zero-config case; `owns()` decides routing before any engine check.
    gateway: Arc<Gateway>,
}

#[tonic::async_trait]
impl NinferenceService for EmbeddedService {
    async fn embed_texts(
        &self,
        request: Request<EmbedTextsRequest>,
    ) -> Result<Response<EmbedTextsResponse>, Status> {
        // The HTTP service layer stamps this before tonic decodes the body;
        // direct unit calls fall back to anchoring here. One instant bounds
        // decode, response-slot wait, validation, construction, admission,
        // execution, and response-tree conversion.
        let deadline_std = request_deadline(&request, self.predict_timeout);
        let deadline = tokio::time::Instant::from_std(deadline_std);
        let req = request.into_inner();
        log::debug!(
            "embedded gRPC: EmbedTexts model={} texts={}",
            req.model,
            req.texts.len()
        );

        // Inference requests NEVER load models: a request-driven load path
        // would let any loopback client expand the resident set past the
        // startup and allow-list bounds. Models become resident at startup
        // or through lifecycle-managed `/admin/load` (`postvec model …`)
        // only; here an unready model is a refusal. Readiness means pool AND
        // executor.
        if deadline <= tokio::time::Instant::now() {
            return Err(Status::deadline_exceeded(
                "deadline exhausted at request entry",
            ));
        }
        // Dispatch order: a ready engine model wins its name, then the
        // gateway, then the MODEL_NOT_LOADED refusal. Two rules meet here:
        // a provider-backed name is never engine-resident, so it must be
        // routed before that refusal fires — and on a name collision the
        // LOCAL model wins, the same §6.1 rule `/config` applies, so
        // discovery and this handler can never disagree about which model a
        // name is. Convert requests never consult the gateway (providers
        // embed; they do not convert).
        //
        // §7.3 admission: the provider path deliberately bypasses all three
        // embedded_max_inflight gates — it took a widened tower slot (see
        // the layer construction in `spawn_inner`), it skips
        // `response_slots`, and it never calls `predict_raw_at` (so
        // HostPolicy is not involved). Provider calls are network-bound and
        // must not queue behind CPU-bound ONNX; their limiter is the
        // per-provider `max_concurrent` semaphore inside the gateway.
        if !self.engine.is_model_ready(&req.model) {
            // `is_model_loaded` and not `is_model_ready` is what decides the
            // collision, because `is_model_loaded` is the predicate `/config`
            // renders from (the engine's model map). A model that is in that
            // map but has not finished building its executor is briefly
            // ready == false, and routing it to the provider there would make
            // discovery and this handler disagree about which model a name
            // is: `/config` would advertise the local dimension while the
            // batch came back in the provider's. Refusing is the honest
            // answer for a load in flight, and MODEL_NOT_LOADED is retried.
            if !self.engine.is_model_loaded(&req.model) && self.gateway.owns(&req.model) {
                return self.embed_via_gateway(req, deadline_std).await;
            }
            return Err(model_not_loaded_status(&req.model));
        }

        // Refuse a text count whose response cannot fit the transport's
        // encode ceiling BEFORE the engine builds the response tree —
        // otherwise the launcher (the process whose death restarts the
        // cluster) materializes the full JSON/protobuf structure only for
        // tonic's encoder to reject it. postvec's own client sub-batches and
        // never trips this; it guards foreign/buggy loopback callers. The
        // dimension is best-effort from the model's configuration (bridge
        // executors carry none — their responses are bounded by the caller's
        // sub-batching and the decode cap on the request).
        // Absolute item ceiling FIRST (round 8): per-item container
        // overhead, not component bytes, is what millions of tiny items
        // cost, and every later guard is O(items).
        if req.texts.len() > crate::jobs::MAX_REQUEST_ITEMS {
            return Err(Status::invalid_argument(format!(
                "{} texts exceeds the {} items-per-request ceiling; split the request",
                req.texts.len(),
                crate::jobs::MAX_REQUEST_ITEMS
            )));
        }
        let dim = resolved_response_dim(
            &self.engine,
            &req.model,
            &req.bridge_model,
            &req.target_model,
        )?
        // Unknown must mean maximally conservative, never unguarded.
        // A request this cap wrongly rejects would have failed
        // resolution in the executor anyway.
        .unwrap_or(16_000);
        let max_items = crate::jobs::max_items_for_dim(dim);
        if req.texts.len() > max_items {
            return Err(Status::invalid_argument(format!(
                "{} texts at {dim} dims exceeds the response envelope; send at most \
                 {max_items} per request",
                req.texts.len()
            )));
        }
        let response_permit =
            tokio::time::timeout_at(deadline, self.response_slots.clone().acquire_owned())
                .await
                .map_err(|_| {
                    Status::deadline_exceeded("deadline exhausted waiting for response capacity")
                })?
                .map_err(|_| Status::unavailable("embedded response-capacity gate closed"))?;

        // Keyed JSON payload — the exact shape the production server builds,
        // including the OpenAI-style optional knobs (wire parity even though
        // postvec's own client never sets them).
        let mut payload = serde_json::Map::new();
        payload.insert(
            "texts".to_string(),
            Value::Array(req.texts.into_iter().map(Value::String).collect()),
        );
        if !req.bridge_model.is_empty() {
            payload.insert("bridge_model".to_string(), Value::String(req.bridge_model));
        }
        if !req.target_model.is_empty() {
            payload.insert("target_model".to_string(), Value::String(req.target_model));
        }
        if !req.encoding_format.is_empty() {
            payload.insert(
                "encoding_format".to_string(),
                Value::String(req.encoding_format),
            );
        }
        if req.dimensions > 0 {
            payload.insert(
                "dimensions".to_string(),
                Value::Number(req.dimensions.into()),
            );
        }
        // §10 option (b): `input_type` is deliberately NOT forwarded to the
        // engine. The extension's client now always sets it (the gateway
        // needs it for Cohere), and the engine applies it only to models
        // that declare templates — but a template-less model (the bundled
        // MiniLM, and every model shipped today) logs a warning PER REQUEST
        // when it sees one, which would flood the PostgreSQL log on every
        // embed, and a templated model's vectors would silently change,
        // which is exactly what the golden-vector suite exists to prevent.
        // Forwarding it is a deliberate, separately tested change (it moves
        // stored-vector semantics); stripping it here preserves today's
        // engine numbers exactly, with no client-side model lookup.
        let _ = req.input_type;
        // `user` is a pass-through identifier; not forwarded (same as the
        // production server).
        let _ = req.user;

        let output = self
            .engine
            .clone()
            .predict_raw_at(
                &req.model,
                InputData::Json(Value::Object(payload)),
                Default::default(),
                deadline_std,
            )
            .await
            .map_err(engine_error_to_status)?;

        let (embeddings, usage) = split_output(output)?;
        let mut response = Response::new(EmbedTextsResponse {
            embeddings: Some(json_to_list_value(embeddings, deadline_std)?),
            usage,
        });
        response
            .extensions_mut()
            .insert(ResponsePermit(Arc::new(response_permit)));
        Ok(response)
    }

    async fn convert_embeddings(
        &self,
        request: Request<ConvertEmbeddingsRequest>,
    ) -> Result<Response<ConvertEmbeddingsResponse>, Status> {
        let deadline_std = request_deadline(&request, self.predict_timeout);
        let deadline = tokio::time::Instant::from_std(deadline_std);
        let req = request.into_inner();
        log::debug!(
            "embedded gRPC: ConvertEmbeddings model={} embeddings={}",
            req.model,
            req.embeddings.len()
        );

        // Same round-8 discipline as EmbedTexts: refuse an exhausted budget
        // and an unready model; never load from a request path.
        if deadline <= tokio::time::Instant::now() {
            return Err(Status::deadline_exceeded(
                "deadline exhausted at request entry",
            ));
        }
        if !self.engine.is_model_ready(&req.model) {
            return Err(model_not_loaded_status(&req.model));
        }

        // Refuse before ANY tree exists. Input side: sum
        // EVERY vector's components — a first-vector-only estimate let
        // ragged input bypass the guard. Output side: resolve the
        // converter's TARGET dimension (direct model param, or the bridge
        // chain's second converter) — 16,000-dim conservative fallback when
        // unknown — and cap the expected response to the wire envelope the
        // encoder will actually accept, so the launcher never builds a
        // response tree that is doomed at the encoder.
        let count = req.embeddings.len() as u64;
        if req.embeddings.len() > crate::jobs::MAX_REQUEST_ITEMS {
            return Err(Status::invalid_argument(format!(
                "{count} embeddings exceeds the {} items-per-request ceiling; split the \
                 request",
                crate::jobs::MAX_REQUEST_ITEMS
            )));
        }
        let total_components: u64 = req.embeddings.iter().map(|v| v.vector.len() as u64).sum();
        // Input-tree accounting charges BOTH sides of the allocation
        // (round 8): ~16 B per float-as-Value component plus
        // [`TREE_ITEM_OVERHEAD_BYTES`] per row for the containers the
        // component estimate cannot see. With the 4096-item ceiling above,
        // the per-item term is bounded (~1 MiB) — it exists so the
        // arithmetic stays honest, not because it can still dominate.
        let estimated_tree_bytes = total_components
            .saturating_mul(INPUT_COMPONENT_TRANSIENT_BYTES)
            .saturating_add(count.saturating_mul(TREE_ITEM_OVERHEAD_BYTES));
        if estimated_tree_bytes > OUTPUT_TREE_BUDGET_BYTES {
            return Err(Status::invalid_argument(format!(
                "{count} embeddings totalling {total_components} components exceeds the \
                 conversion input envelope; split the request"
            )));
        }
        let out_dim = resolved_response_dim(
            &self.engine,
            &req.model,
            &req.bridge_model,
            &req.target_model,
        )?
        .unwrap_or(16_000);
        let max_items = crate::jobs::max_items_for_dim(out_dim);
        if req.embeddings.len() > max_items {
            return Err(Status::invalid_argument(format!(
                "{count} embeddings at a {out_dim}-dim target exceeds the response \
                 envelope; send at most {max_items} per request"
            )));
        }
        let response_permit =
            tokio::time::timeout_at(deadline, self.response_slots.clone().acquire_owned())
                .await
                .map_err(|_| {
                    Status::deadline_exceeded("deadline exhausted waiting for response capacity")
                })?
                .map_err(|_| Status::unavailable("embedded response-capacity gate closed"))?;

        // Positional payload; JSON numbers cannot carry NaN/Inf, reject
        // explicitly (same contract as the remote server).
        let mut rows = Vec::with_capacity(req.embeddings.len());
        let mut components_built = 0u64;
        let mut next_deadline_check = DEADLINE_CHECK_COMPONENTS;
        for fv in req.embeddings {
            let mut row = Vec::with_capacity(fv.vector.len());
            for f in fv.vector {
                components_built += 1;
                if components_built >= next_deadline_check {
                    if deadline <= tokio::time::Instant::now() {
                        return Err(Status::deadline_exceeded(
                            "deadline exhausted while building the conversion payload",
                        ));
                    }
                    next_deadline_check =
                        components_built.saturating_add(DEADLINE_CHECK_COMPONENTS);
                }
                let Some(n) = serde_json::Number::from_f64(f as f64) else {
                    return Err(invalid_input_status(
                        "ConvertEmbeddings input contains a non-finite component (NaN/Inf)",
                    ));
                };
                row.push(Value::Number(n));
            }
            rows.push(Value::Array(row));
        }
        let mut inputs = vec![Value::Array(rows)];
        for field in [req.source_model, req.bridge_model, req.target_model] {
            if !field.is_empty() {
                inputs.push(Value::String(field));
            }
        }

        let output = self
            .engine
            .clone()
            .predict_raw_at(
                &req.model,
                InputData::Structured(inputs),
                Default::default(),
                deadline_std,
            )
            .await
            .map_err(engine_error_to_status)?;

        let (embeddings, usage) = split_output(output)?;
        let mut response = Response::new(ConvertEmbeddingsResponse {
            embeddings: Some(json_to_list_value(embeddings, deadline_std)?),
            usage,
        });
        response
            .extensions_mut()
            .insert(ResponsePermit(Arc::new(response_permit)));
        Ok(response)
    }
}

impl EmbeddedService {
    /// The provider path of `EmbedTexts` (§7.3): dispatch to the gateway,
    /// bounded by the per-provider semaphore and the caller's deadline —
    /// never by `response_slots` or the engine's HostPolicy (a provider
    /// call is network-bound and must not serialize behind local ONNX).
    /// The response envelope is still enforced with the same math as the
    /// engine path, sized from the descriptor's declared dimension.
    async fn embed_via_gateway(
        &self,
        req: EmbedTextsRequest,
        deadline: std::time::Instant,
    ) -> Result<Response<EmbedTextsResponse>, Status> {
        if req.texts.len() > crate::jobs::MAX_REQUEST_ITEMS {
            return Err(Status::invalid_argument(format!(
                "{} texts exceeds the {} items-per-request ceiling; split the request",
                req.texts.len(),
                crate::jobs::MAX_REQUEST_ITEMS
            )));
        }
        // The gateway validates every returned vector against this dim, so
        // the envelope math is exact, not best-effort.
        let dim = self
            .gateway
            .dim(&req.model)
            .map(|d| d as i32)
            .unwrap_or(16_000);
        let max_items = crate::jobs::max_items_for_dim(dim);
        if req.texts.len() > max_items {
            return Err(Status::invalid_argument(format!(
                "{} texts at {dim} dims exceeds the response envelope; send at most \
                 {max_items} per request",
                req.texts.len()
            )));
        }

        // Taken BEFORE the call, so no paid embedding is ever thrown away for
        // want of a slot, and released only when the encoded body is dropped
        // (see `ResponsePermit` below). Bounded by the caller's deadline like
        // every other wait on this path.
        let response_permit = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.provider_response_slots.clone().acquire_owned(),
        )
        .await
        .map_err(|_| {
            Status::deadline_exceeded("deadline exhausted waiting for provider response capacity")
        })?
        .map_err(|_| Status::unavailable("provider response-capacity gate closed"))?;

        let input_type = InputType::from_wire(&req.input_type);
        let vectors = self
            .gateway
            .embed(&req.model, &req.texts, input_type, deadline)
            .await
            .map_err(gateway_error_to_status)?;

        let mut response = Response::new(EmbedTextsResponse {
            embeddings: Some(vectors_to_list_value(vectors, deadline)?),
            usage: None,
        });
        // The permit rides the response body: `ResponsePermitLayer` moves it
        // into `PermitBody`, which holds it until hyper finishes or drops the
        // encoding. This is the whole fix — a completed provider tree is not
        // "done" until the bytes leave.
        response
            .extensions_mut()
            .insert(ResponsePermit(Arc::new(response_permit)));
        Ok(response)
    }
}

/// [`GatewayError`] → tonic Status + `x-ravenna-error-code` metadata, the
/// same wire contract as [`engine_error_to_status`] — postvec's client (and
/// any other) classifies provider failures exactly like a remote node's
/// answers, per the §6.4 mapping table. Messages are secret-free by
/// construction in the providers crate.
fn gateway_error_to_status(e: GatewayError) -> Status {
    let code = e.code;
    let message = e.message;
    log::warn!(
        "embedded gRPC provider request failed [{}]: {message}",
        code.as_str()
    );
    let mut status = match code {
        shared::ErrorCode::Timeout => Status::deadline_exceeded(message),
        shared::ErrorCode::ModelNotFound => Status::not_found(message),
        shared::ErrorCode::InvalidInput | shared::ErrorCode::ContextLengthExceeded => {
            Status::invalid_argument(message)
        }
        shared::ErrorCode::UpstreamAuthFailed => Status::unauthenticated(message),
        shared::ErrorCode::UpstreamServiceUnavailable => Status::unavailable(message),
        _ => Status::internal(message),
    };
    status.metadata_mut().insert(
        "x-ravenna-error-code",
        tonic::metadata::MetadataValue::from_static(code.as_str()),
    );
    status
}

/// Gateway vectors → prost `ListValue[ListValue[NumberValue]]`, the direct
/// twin of [`json_to_list_value`] without the intermediate JSON tree (the
/// gateway already validated count and dimension). The same output-tree
/// budget and deadline cadence apply.
#[allow(clippy::result_large_err)]
fn vectors_to_list_value(
    vectors: Vec<Vec<f32>>,
    deadline: std::time::Instant,
) -> Result<ListValue, Status> {
    let total_components: u64 = vectors.iter().map(|v| v.len() as u64).sum();
    let estimated = total_components
        .saturating_mul(PROST_COMPONENT_BYTES)
        .saturating_add((vectors.len() as u64).saturating_mul(TREE_ITEM_OVERHEAD_BYTES));
    if estimated > OUTPUT_TREE_BUDGET_BYTES {
        return Err(resource_exhausted_status(format!(
            "provider output of {} rows / {total_components} components exceeds the \
             embedded response-tree envelope",
            vectors.len()
        )));
    }
    let mut prost_outer = Vec::with_capacity(vectors.len());
    let mut converted = 0u64;
    let mut next_deadline_check = DEADLINE_CHECK_COMPONENTS;
    for vector in vectors {
        let mut prost_inner = Vec::with_capacity(vector.len());
        for f in vector {
            converted += 1;
            if converted >= next_deadline_check {
                if std::time::Instant::now() >= deadline {
                    return Err(Status::deadline_exceeded(
                        "deadline exhausted while encoding the provider response",
                    ));
                }
                next_deadline_check = converted.saturating_add(DEADLINE_CHECK_COMPONENTS);
            }
            prost_inner.push(ProstValue {
                kind: Some(prost_types::value::Kind::NumberValue(f as f64)),
            });
        }
        prost_outer.push(ProstValue {
            kind: Some(prost_types::value::Kind::ListValue(ListValue {
                values: prost_inner,
            })),
        });
    }
    Ok(ListValue {
        values: prost_outer,
    })
}

/// Stored in tonic response extensions by the handler, then moved into the
/// actual HTTP body by [`ResponsePermitLayer`].
#[derive(Clone, Debug)]
struct ResponsePermit(#[allow(dead_code)] Arc<OwnedSemaphorePermit>);

/// Body wrapper whose only extra responsibility is owning a response permit
/// until hyper finishes or drops lazy gRPC encoding.
struct PermitBody<B> {
    inner: Pin<Box<B>>,
    _permit: Option<ResponsePermit>,
}

impl<B: Body> Body for PermitBody<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.inner.as_mut().poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

/// HTTP-level layer around tonic. It stamps the absolute deadline before
/// protobuf decode and transfers the handler's response permit from response
/// extensions into the lazy body.
#[derive(Clone)]
struct ResponsePermitLayer {
    predict_timeout: Duration,
}

#[derive(Clone)]
struct ResponsePermitService<S> {
    inner: S,
    predict_timeout: Duration,
}

impl<S> Layer<S> for ResponsePermitLayer {
    type Service = ResponsePermitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ResponsePermitService {
            inner,
            predict_timeout: self.predict_timeout,
        }
    }
}

impl<S, ReqBody, ResBody> Service<tonic::codegen::http::Request<ReqBody>>
    for ResponsePermitService<S>
where
    S: Service<
        tonic::codegen::http::Request<ReqBody>,
        Response = tonic::codegen::http::Response<ResBody>,
    >,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    ResBody: Body + 'static,
{
    type Response = tonic::codegen::http::Response<PermitBody<ResBody>>;
    type Error = S::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: tonic::codegen::http::Request<ReqBody>) -> Self::Future {
        let budget = request
            .headers()
            .get("grpc-timeout")
            .and_then(|value| value.to_str().ok())
            .and_then(parse_grpc_timeout)
            .map_or(self.predict_timeout, |d| d.min(self.predict_timeout));
        request
            .extensions_mut()
            .insert(AbsoluteDeadline(deadline_from_budget(
                std::time::Instant::now(),
                budget,
            )));
        let future = self.inner.call(request);
        Box::pin(async move {
            let mut response = future.await?;
            let permit = response.extensions_mut().remove::<ResponsePermit>();
            let (parts, body) = response.into_parts();
            Ok(tonic::codegen::http::Response::from_parts(
                parts,
                PermitBody {
                    inner: Box::pin(body),
                    _permit: permit,
                },
            ))
        })
    }
}

/// Spawn the server on the engine runtime. Binding happens synchronously so
/// a taken port / bad address fails the init attempt with a precise error
/// instead of a silent dead listener.
pub(super) fn spawn(
    engine: Arc<InferenceEngine>,
    runtime: &tokio::runtime::Runtime,
    listen: &str,
    predict_timeout: Duration,
    max_inflight: usize,
    gateway: Arc<Gateway>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    spawn_inner(
        engine,
        runtime,
        listen,
        predict_timeout,
        max_inflight,
        gateway,
    )
    .map(|(handle, _)| handle)
}

/// Test hook: also reports the bound address (port 0 support).
#[cfg(test)]
pub(super) fn spawn_for_test(
    engine: Arc<InferenceEngine>,
    runtime: &tokio::runtime::Runtime,
    listen: &str,
    predict_timeout: Duration,
    gateway: Arc<Gateway>,
) -> Result<(tokio::task::JoinHandle<()>, SocketAddr), String> {
    spawn_inner(engine, runtime, listen, predict_timeout, 1, gateway)
}

fn spawn_inner(
    engine: Arc<InferenceEngine>,
    runtime: &tokio::runtime::Runtime,
    listen: &str,
    predict_timeout: Duration,
    max_inflight: usize,
    gateway: Arc<Gateway>,
) -> Result<(tokio::task::JoinHandle<()>, SocketAddr), String> {
    let addr: SocketAddr = listen
        .parse()
        .map_err(|e| format!("invalid postvec.embedded_listen {listen:?}: {e}"))?;
    if !addr.ip().is_loopback() {
        // postvec's gRPC has no auth/TLS — anything that can reach this
        // socket can drive inference. A non-loopback bind is therefore
        // refused, not merely flagged. Failing engine init (the worker
        // retries and reports it in stats().last_error) is strictly
        // better than silently serving the network.
        return Err(format!(
            "postvec.embedded_listen {listen} is not a loopback address; the embedded gRPC \
             server has no authentication and only ever binds loopback"
        ));
    }

    // Bind inside the runtime: tokio's listener registers with that
    // runtime's reactor at construction.
    let incoming = runtime
        .block_on(async { tonic::transport::server::TcpIncoming::bind(addr) })
        .map_err(|e| format!("bind {addr}: {e}"))?;
    let bound = incoming
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;

    let response_slots = Arc::new(Semaphore::new(max_inflight.max(1)));
    // §7.3 (recommended default): the tower ingress limit stays as the
    // decode-amplification backstop, widened by the sum of the configured
    // per-provider `max_concurrent` caps so provider calls (network-bound)
    // never queue behind CPU-bound ONNX at a gate they don't need. The
    // budget is read from the gateway loaded at startup; a runtime
    // `/admin/providers/reload` that RAISES the budget shares the startup
    // ingress width until the next restart (serving is correct, admission
    // is merely tighter — the reload endpoint logs a warning naming the
    // restart). Zero-config: budget 0, size unchanged, so the
    // documented ≤ max_inflight × ~320 MiB ingress RSS bound holds exactly.
    // Residual (deliberate): a burst of *engine* EmbedTexts can occupy the
    // extra tower slots, decode, then wait on `response_slots` — extra
    // decoded RSS exists only when providers are configured, and callers
    // still carry `grpc-timeout`.
    let ingress_limit = max_inflight.max(1) + gateway.inflight_budget();
    // The response-lifetime bound for the provider path. Same width as the
    // ingress widening above and for the same reason: the per-provider
    // semaphores already cap concurrent provider *calls* at exactly this
    // number, so this can only ever be contended by responses that have
    // outlived their handler — which is what it exists to bound.
    let provider_response_slots = Arc::new(Semaphore::new(gateway.inflight_budget().max(1)));
    let service = NinferenceServiceServer::new(EmbeddedService {
        engine,
        predict_timeout,
        response_slots,
        provider_response_slots,
        gateway,
    })
    .max_encoding_message_size(MAX_ENCODE_MESSAGE_SIZE)
    .max_decoding_message_size(MAX_DECODE_MESSAGE_SIZE);

    let handle = runtime.spawn(async move {
        if let Err(e) = tonic::transport::Server::builder()
            // Global in-flight request cap: every
            // PostgreSQL backend owns its own connection, so a per-connection
            // limit alone multiplies with the backend count. The shared
            // semaphore gates requests across ALL connections before the
            // handler touches the body, and its size is
            // `postvec.embedded_max_inflight` — the same knob that gates
            // engine admission — so requests the engine cannot run anyway
            // never sit decoded in memory. That makes the launcher's
            // transient ingress RSS ≤ max_inflight x (64 MiB decoded
            // request + ~96 MiB input tree + ~96 MiB response tree + 64 MiB
            // encoded response) ≈ 320 MiB at the default of 1. Residual: a
            // single in-flight message may still decode up to the 64 MiB
            // ceiling.
            // The deadline layer sits OUTSIDE the concurrency limit: the
            // absolute deadline is stamped at ingress, so time spent queued
            // behind the global cap is charged against the caller's
            // `grpc-timeout` — a request admitted after its caller gave up
            // is refused by the handler's exhausted-budget pre-check instead
            // of starting a fresh full budget of native work.
            .layer(ResponsePermitLayer { predict_timeout })
            .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
                ingress_limit,
            ))
            .concurrency_limit_per_connection(4)
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
        {
            // Engine-runtime thread: log-crate only (never pgrx elog here).
            log::error!("postvec embedded gRPC server exited: {e}");
        }
    });
    Ok((handle, bound))
}

/// `EngineError` → tonic Status + `x-ravenna-error-code` metadata: the same
/// mapping the production gRPC server applies for engine
/// errors, so postvec's (and any other) client classifies identically.
fn engine_error_to_status(e: EngineError) -> Status {
    let code = e.to_error_code();
    let message = e.to_string();
    log::warn!(
        "embedded gRPC request failed [{}]: {message}",
        code.as_str()
    );
    let mut status = match code {
        shared::ErrorCode::ModelNotFound => Status::not_found(message),
        shared::ErrorCode::InvalidInput => Status::invalid_argument(message),
        shared::ErrorCode::Timeout => Status::deadline_exceeded(message),
        shared::ErrorCode::TargetRestricted => Status::failed_precondition(message),
        shared::ErrorCode::BridgePathNotFound | shared::ErrorCode::ConverterNotFound => {
            Status::failed_precondition(message)
        }
        _ => Status::internal(message),
    };
    status.metadata_mut().insert(
        "x-ravenna-error-code",
        tonic::metadata::MetadataValue::from_static(code.as_str()),
    );
    status
}

/// The refusal for a request naming a model that is not resident+ready:
/// `FailedPrecondition` + `MODEL_NOT_LOADED`, pointing at the only
/// legitimate load paths. FailedPrecondition (not NotFound) because the
/// model may exist on disk — the caller's next step is an admin action,
/// not a different name.
#[allow(clippy::result_large_err)]
fn model_not_loaded_status(model: &str) -> Status {
    let mut status = Status::failed_precondition(format!(
        "model {model:?} is not loaded in the embedded engine; load it via `postvec model \
         activate`/`postvec.embedded_models` — inference requests never load models"
    ));
    status.metadata_mut().insert(
        "x-ravenna-error-code",
        tonic::metadata::MetadataValue::from_static(shared::ErrorCode::ModelNotLoaded.as_str()),
    );
    status
}

fn invalid_input_status(message: impl Into<String>) -> Status {
    let mut status = Status::invalid_argument(message.into());
    status.metadata_mut().insert(
        "x-ravenna-error-code",
        tonic::metadata::MetadataValue::from_static(shared::ErrorCode::InvalidInput.as_str()),
    );
    status
}

fn resource_exhausted_status(message: impl Into<String>) -> Status {
    let mut status = Status::resource_exhausted(message.into());
    // A response-shape/envelope mismatch is deterministic for this model and
    // batch, not a transport blip to retry forever.
    status.metadata_mut().insert(
        "x-ravenna-error-code",
        tonic::metadata::MetadataValue::from_static(shared::ErrorCode::InvalidInput.as_str()),
    );
    status
}

/// Internal error with `INTERNAL_ERROR` metadata — used where the remote
/// server routes marshalling failures through `app_error_to_status` (which
/// attaches the code), so the wire stays byte-parity with a real nin node.
fn internal_status(message: impl Into<String>) -> Status {
    let mut status = Status::internal(message.into());
    status.metadata_mut().insert(
        "x-ravenna-error-code",
        tonic::metadata::MetadataValue::from_static(shared::ErrorCode::InternalError.as_str()),
    );
    status
}

/// First structured output + optional usage out of an `ExecutorOutput`.
// A `Status` Err is what the tonic handlers need to `?` on; its size is
// tonic's business (the handler signatures carry it anyway).
#[allow(clippy::result_large_err)]
fn split_output(output: ExecutorOutput) -> Result<(Value, Option<TokenUsage>), Status> {
    match output {
        ExecutorOutput::Structured(mut outputs) => {
            if outputs.is_empty() {
                return Err(Status::internal(
                    "Executor returned 0 outputs, but gRPC requires 1.",
                ));
            }
            Ok((outputs.remove(0), None))
        }
        ExecutorOutput::StructuredWithUsage { mut outputs, usage } => {
            if outputs.is_empty() {
                return Err(Status::internal(
                    "Executor returned 0 outputs, but gRPC requires 1.",
                ));
            }
            Ok((
                outputs.remove(0),
                Some(TokenUsage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                }),
            ))
        }
        ExecutorOutput::Json(_) => Err(Status::internal(
            "Executor returned unexpected `Json` output; `Structured` was expected.",
        )),
        ExecutorOutput::Binary { .. } => Err(Status::internal(
            "Executor returned binary output, which is not supported.",
        )),
    }
}

/// `[[f32]]` JSON → prost `ListValue[ListValue[NumberValue]]` (the port of
/// the production server's `parse_embeddings_to_list_value`). The engine JSON and prost
/// trees coexist during this conversion, so validate the ACTUAL output shape
/// and combined allocation estimate before creating the second tree. This is
/// the post-execution backstop for a descriptor whose declared target_dim does
/// not match what its executor returned.
#[allow(clippy::result_large_err)]
fn json_to_list_value(
    embeddings: Value,
    deadline: std::time::Instant,
) -> Result<ListValue, Status> {
    if std::time::Instant::now() >= deadline {
        return Err(Status::deadline_exceeded(
            "deadline exhausted before encoding the embedding response",
        ));
    }
    let outer = embeddings
        .as_array()
        .ok_or_else(|| internal_status("Executor output was not a JSON array."))?;
    if outer.len() > crate::jobs::MAX_REQUEST_ITEMS {
        return Err(resource_exhausted_status(format!(
            "executor returned {} embedding rows; ceiling is {}",
            outer.len(),
            crate::jobs::MAX_REQUEST_ITEMS
        )));
    }
    let mut total_components = 0u64;
    for vec_value in outer {
        let inner = vec_value
            .as_array()
            .ok_or_else(|| internal_status("Embedding element was not an array of numbers."))?;
        total_components = total_components.saturating_add(inner.len() as u64);
    }
    let estimated = total_components
        .saturating_mul(OUTPUT_COMPONENT_TRANSIENT_BYTES)
        .saturating_add((outer.len() as u64).saturating_mul(TREE_ITEM_OVERHEAD_BYTES));
    if estimated > OUTPUT_TREE_BUDGET_BYTES {
        return Err(resource_exhausted_status(format!(
            "executor output of {} rows / {total_components} components exceeds the \
             embedded response-tree envelope",
            outer.len()
        )));
    }

    let mut prost_outer = Vec::with_capacity(outer.len());
    let mut converted = 0u64;
    let mut next_deadline_check = DEADLINE_CHECK_COMPONENTS;
    for vec_value in outer {
        let inner = vec_value
            .as_array()
            .ok_or_else(|| internal_status("Embedding element was not an array of numbers."))?;
        let mut prost_inner = Vec::with_capacity(inner.len());
        for num in inner {
            converted += 1;
            if converted >= next_deadline_check {
                if std::time::Instant::now() >= deadline {
                    return Err(Status::deadline_exceeded(
                        "deadline exhausted while encoding the embedding response",
                    ));
                }
                next_deadline_check = converted.saturating_add(DEADLINE_CHECK_COMPONENTS);
            }
            let f = num
                .as_f64()
                .ok_or_else(|| internal_status("Embedding vector element was not a number."))?;
            prost_inner.push(ProstValue {
                kind: Some(prost_types::value::Kind::NumberValue(f)),
            });
        }
        prost_outer.push(ProstValue {
            kind: Some(prost_types::value::Kind::ListValue(ListValue {
                values: prost_inner,
            })),
        });
    }
    Ok(ListValue {
        values: prost_outer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::value::Kind;
    use serde_json::json;

    fn code_meta(status: &Status) -> String {
        status
            .metadata()
            .get("x-ravenna-error-code")
            .expect("error code metadata present")
            .to_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn engine_errors_carry_wire_metadata() {
        let status = engine_error_to_status(EngineError::NotFound("model 'foo' not found".into()));
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert_eq!(code_meta(&status), "MODEL_NOT_FOUND");

        let status = engine_error_to_status(EngineError::TargetRestricted(
            "embed-bridge target 'x' is not available".into(),
        ));
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(code_meta(&status), "TARGET_RESTRICTED");

        let status = engine_error_to_status(EngineError::BridgePathNotFound(
            "bridge chain is incomplete".into(),
        ));
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(code_meta(&status), "BRIDGE_PATH_NOT_FOUND");

        let status = engine_error_to_status(EngineError::ConverterNotFound(
            "converter is missing".into(),
        ));
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(code_meta(&status), "CONVERTER_NOT_FOUND");

        // Round 8: an exhausted engine budget is a typed timeout and must
        // cross the wire as DeadlineExceeded, never Internal.
        let status = engine_error_to_status(EngineError::Timeout("budget exhausted".into()));
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
        assert_eq!(code_meta(&status), "TIMEOUT");

        let status = engine_error_to_status(EngineError::Prediction("kaboom".into()));
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(code_meta(&status), "INTERNAL_ERROR");

        let status = invalid_input_status("bad vector");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(code_meta(&status), "INVALID_INPUT");
    }

    #[test]
    fn json_to_list_value_builds_nested_lists() {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let lv = json_to_list_value(json!([[1.0, 2.0], [3.0]]), deadline).unwrap();
        assert_eq!(lv.values.len(), 2);
        let Some(Kind::ListValue(inner)) = &lv.values[0].kind else {
            panic!("expected nested ListValue");
        };
        assert_eq!(inner.values.len(), 2);
        assert!(matches!(inner.values[0].kind, Some(Kind::NumberValue(n)) if n == 1.0));

        // Marshalling failures carry INTERNAL_ERROR metadata — the remote
        // server routes them through app_error_to_status, which attaches it.
        for bad in [json!("nope"), json!([1, 2]), json!([["a"]])] {
            let status = json_to_list_value(bad, deadline).unwrap_err();
            assert_eq!(status.code(), tonic::Code::Internal);
            assert_eq!(code_meta(&status), "INTERNAL_ERROR");
        }

        let too_many = Value::Array(
            (0..=crate::jobs::MAX_REQUEST_ITEMS)
                .map(|_| json!([]))
                .collect(),
        );
        let status = json_to_list_value(too_many, deadline).unwrap_err();
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert_eq!(code_meta(&status), "INVALID_INPUT");

        let status = json_to_list_value(json!([[1.0]]), std::time::Instant::now()).unwrap_err();
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
    }

    #[test]
    fn response_permit_is_owned_by_the_lazy_http_body() {
        use std::convert::Infallible;

        struct ReadyResponse(Option<ResponsePermit>);
        impl Service<tonic::codegen::http::Request<()>> for ReadyResponse {
            type Response = tonic::codegen::http::Response<tonic::body::Body>;
            type Error = Infallible;
            type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

            fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, _request: tonic::codegen::http::Request<()>) -> Self::Future {
                let mut response = tonic::codegen::http::Response::new(tonic::body::Body::empty());
                response
                    .extensions_mut()
                    .insert(self.0.take().expect("one call"));
                std::future::ready(Ok(response))
            }
        }

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let slots = Arc::new(Semaphore::new(1));
            let permit = slots.clone().try_acquire_owned().unwrap();
            let mut service = ResponsePermitLayer {
                predict_timeout: Duration::from_secs(5),
            }
            .layer(ReadyResponse(Some(ResponsePermit(Arc::new(permit)))));
            futures_util::future::poll_fn(|cx| service.poll_ready(cx))
                .await
                .unwrap();
            let response = service
                .call(tonic::codegen::http::Request::new(()))
                .await
                .unwrap();
            assert_eq!(slots.available_permits(), 0, "body retains the permit");
            drop(response);
            assert_eq!(
                slots.available_permits(),
                1,
                "dropping the body releases it"
            );
        });
    }

    #[test]
    fn split_output_handles_all_variants() {
        let (v, usage) = split_output(ExecutorOutput::Structured(vec![json!([[1.0]])])).unwrap();
        assert_eq!(v, json!([[1.0]]));
        assert!(usage.is_none());

        let (_, usage) = split_output(ExecutorOutput::StructuredWithUsage {
            outputs: vec![json!([[1.0]])],
            usage: engine::executors::ExecutionMetadata {
                prompt_tokens: 3,
                completion_tokens: 0,
                total_tokens: 3,
            },
        })
        .unwrap();
        assert_eq!(usage.unwrap().prompt_tokens, 3);

        assert!(split_output(ExecutorOutput::Structured(vec![])).is_err());
        assert!(split_output(ExecutorOutput::Json(json!({}))).is_err());
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    /// The exhausted-budget pre-check must run before any other work.
    /// Inference RPCs never load models (an unready model is
    /// `MODEL_NOT_LOADED`), so with an expired inbound `grpc-timeout` the
    /// handler must answer DeadlineExceeded — not the readiness error the
    /// missing model would otherwise produce, which would prove work ran
    /// after the budget was gone.
    #[test]
    fn expired_inbound_deadline_refuses_before_model_load() {
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);
        let svc = EmbeddedService {
            engine,
            predict_timeout: Duration::from_secs(30),
            response_slots: Arc::new(Semaphore::new(1)),
            provider_response_slots: Arc::new(Semaphore::new(4)),
            gateway: Arc::new(Gateway::empty()),
        };

        let mut req = Request::new(EmbedTextsRequest {
            model: "no-such-model".to_string(),
            texts: vec!["x".to_string()],
            ..Default::default()
        });
        req.metadata_mut()
            .insert("grpc-timeout", "1n".parse().unwrap());
        let err = runtime.block_on(svc.embed_texts(req)).unwrap_err();
        assert_eq!(
            err.code(),
            tonic::Code::DeadlineExceeded,
            "an exhausted deadline must refuse before load, got: {err:?}"
        );

        let mut req = Request::new(ConvertEmbeddingsRequest {
            model: "no-such-converter".to_string(),
            ..Default::default()
        });
        req.metadata_mut()
            .insert("grpc-timeout", "1n".parse().unwrap());
        let err = runtime.block_on(svc.convert_embeddings(req)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::DeadlineExceeded, "got: {err:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A resolved-but-implausible target dimension is a refusal, never an
    /// input to `as i32` / response sizing (round 7).
    #[test]
    fn implausible_resolved_dimension_is_refused() {
        let status = invalid_input_status(format!(
            "model \"m\" reports an implausible target dimension {}; refusing to size a \
             response from it",
            MAX_PLAUSIBLE_DIM + 1
        ));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}

#[cfg(test)]
mod gateway_tests {
    use super::*;
    use crate::client::grpc::GrpcClient;
    use crate::client::{
        ConvertRoute, EmbedRoute, ErrorClass, InferenceClient, PvError, RavennaCode,
    };
    use providers::testing as provider_mock;

    /// A completed unary response is encoded *lazily*, after the handler
    /// future has returned — so by the time the bytes are written, the
    /// per-provider semaphore and the tower ingress permit are both long
    /// released. Without a response-lifetime permit, a slow or abandoned
    /// loopback reader could accumulate finished provider trees without
    /// limit, and here that is the PostgreSQL launcher's RSS.
    ///
    /// The engine path has held such a permit since it was written; this is
    /// the same mechanism on its own budget.
    #[test]
    fn a_provider_response_holds_its_permit_until_the_body_is_dropped() {
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);
        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        let slots = Arc::new(Semaphore::new(1));
        let service = EmbeddedService {
            engine,
            predict_timeout: Duration::from_secs(5),
            // Zero engine permits: the provider path must not touch them.
            response_slots: Arc::new(Semaphore::new(0)),
            provider_response_slots: slots.clone(),
            gateway: gateway_for(&mock.url),
        };

        let response = runtime
            .block_on(service.embed_texts(tonic::Request::new(EmbedTextsRequest {
                model: "openai-text-embedding-3-small".to_string(),
                texts: vec!["hello".to_string()],
                ..Default::default()
            })))
            .expect("provider embed");

        // The handler has returned and the vectors are in hand — and the
        // budget is still spent, because the response has not been written.
        assert_eq!(
            slots.available_permits(),
            0,
            "a finished-but-unsent provider response must still hold its permit"
        );
        assert!(response.extensions().get::<ResponsePermit>().is_some());
        drop(response);
        assert_eq!(slots.available_permits(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A providers.d directory serving one 2-dim OpenAI-typed model pointed
    /// at `base_url`, loaded into a gateway.
    fn gateway_for(base_url: &str) -> Arc<Gateway> {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "postvec-gwtest-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "-")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // A providers.d is 0700; the loader refuses a group/world-writable
        // one, and `create_dir_all` honours the umask.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("openai.toml");
        std::fs::write(
            &path,
            format!(
                "provider = \"openai\"\napi_key = \"sk-test\"\nbase_url = \"{base_url}\"\n\n\
                 [[models]]\nname = \"openai-text-embedding-3-small\"\n\
                 provider_model_id = \"text-embedding-3-small\"\ndim = 2\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let gateway = Arc::new(Gateway::load(&dir));
        let _ = std::fs::remove_dir_all(&dir);
        gateway
    }

    /// The full wire path: postvec's production `GrpcClient` → loopback
    /// server → gateway → mock provider. The engine holds zero models, so
    /// success is itself the ordering proof — the gateway must be consulted
    /// before the MODEL_NOT_LOADED refusal, or every provider name would
    /// refuse. Convert never takes the gateway branch.
    #[test]
    fn provider_embed_serves_before_engine_readiness_and_convert_never_does() {
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);

        // 0.25/0.5 are exact in f32→f64→f32, so the prost round trip is
        // byte-stable.
        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        let gateway = gateway_for(&mock.url);

        let (server, addr) = spawn_for_test(
            engine,
            &runtime,
            "127.0.0.1:0",
            Duration::from_secs(5),
            gateway,
        )
        .unwrap();
        let client = GrpcClient::new(vec![addr.to_string()], Vec::new(), 5_000, 1_000);

        let out = crate::runtime::block_on(client.embed(
            &["hello".to_string()],
            "openai-text-embedding-3-small",
            &EmbedRoute::default(),
        ))
        .expect("provider model serves through the wire");
        assert_eq!(out, vec![vec![0.25, 0.5]]);

        // Convert requests never touch the gateway: the same name refuses
        // exactly like any model the engine does not hold.
        let err = crate::runtime::block_on(client.convert(
            &[vec![1.0f32, 2.0]],
            "openai-text-embedding-3-small",
            &ConvertRoute::default(),
        ))
        .unwrap_err();
        assert!(
            matches!(&err, PvError::Remote { code, .. } if *code == RavennaCode::ModelNotLoaded),
            "got {err:?}"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The §6.1 collision rule on the EMBED path, not just in `/config`: a
    /// loaded local model wins its name, and the provider claiming it is
    /// never dialed. The local fixture is a dummy-executor model, whose
    /// distinctive engine-side error doubles as proof of which path served
    /// the request.
    #[test]
    fn a_loaded_local_model_wins_the_name_collision_on_the_embed_path() {
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);

        // A real (dummy-executor) engine model under the colliding name.
        let name = "openai-text-embedding-3-small";
        let dir = root.join("models").join("generic").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("ninference.hub.json"),
            serde_json::json!({
                "name": name,
                "backend": "generic",
                "enabled": true,
                "executor": { "key": "dummy" }
            })
            .to_string(),
        )
        .unwrap();
        runtime.block_on(engine.load_model(name)).unwrap();
        assert!(engine.is_model_ready(name), "local fixture is resident");

        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        let gateway = gateway_for(&mock.url);
        assert!(gateway.owns(name), "the provider file claims the name");

        let (server, addr) = spawn_for_test(
            engine,
            &runtime,
            "127.0.0.1:0",
            Duration::from_secs(5),
            gateway,
        )
        .unwrap();
        let client = GrpcClient::new(vec![addr.to_string()], Vec::new(), 5_000, 1_000);

        let err = crate::runtime::block_on(client.embed(
            &["hello".to_string()],
            name,
            &EmbedRoute::default(),
        ))
        .unwrap_err();
        // The dummy executor's error proves the ENGINE served the name —
        // exactly what /config advertises (local wins).
        let message = format!("{err}");
        assert!(message.contains("dummy executor"), "got: {message}");
        assert_eq!(
            mock.request_count(),
            0,
            "the provider must never be dialed for a local-won name"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A reload that adds the FIRST provider (zero-config start, then
    /// `provider add`) serves through the wire without a restart. The tower
    /// ingress width stays at its spawn-time size until restart — that
    /// residual is a logged warning, not a serving failure.
    #[test]
    fn reload_that_adds_the_first_provider_serves_through_the_wire() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);

        let providers_dir = root.join("providers.d");
        let gateway = Arc::new(Gateway::load(&providers_dir));
        assert!(gateway.is_empty(), "zero-config start");

        let (server, addr) = spawn_for_test(
            engine,
            &runtime,
            "127.0.0.1:0",
            Duration::from_secs(5),
            gateway.clone(),
        )
        .unwrap();
        let client = GrpcClient::new(vec![addr.to_string()], Vec::new(), 5_000, 1_000);

        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        std::fs::create_dir_all(&providers_dir).unwrap();
        // A providers.d is 0700; the loader refuses a group/world-writable
        // one, and `create_dir_all` honours the umask.
        std::fs::set_permissions(&providers_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = providers_dir.join("openai.toml");
        std::fs::write(
            &file,
            format!(
                "provider = \"openai\"\napi_key = \"sk-test\"\nbase_url = \"{}\"\n\n\
                 [[models]]\nname = \"openai-text-embedding-3-small\"\n\
                 provider_model_id = \"text-embedding-3-small\"\ndim = 2\n",
                mock.url
            ),
        )
        .unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let report = gateway.reload(&providers_dir).unwrap();
        assert_eq!(report.models, 1, "{:?}", report.errors);

        let out = crate::runtime::block_on(client.embed(
            &["hello".to_string()],
            "openai-text-embedding-3-small",
            &EmbedRoute::default(),
        ))
        .expect("a reloaded-in provider serves without a restart");
        assert_eq!(out, vec![vec![0.25, 0.5]]);

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 6, end to end: the purpose on `EmbedRoute` becomes
    /// `EmbedTextsRequest.input_type` on the wire, and the gateway turns it
    /// into the provider's own vocabulary. Cohere is the connector whose
    /// request body carries it, so its mock request is the observable.
    ///
    /// This is the honest form of "search sets Query": `search()` embeds
    /// through `embed_texts`, which constructs its own `GrpcClient` with no
    /// injection seam, and `pg_test` has no host at all — so the contract is
    /// proved here, through the real client, the real server and the real
    /// gateway, rather than against a mock two layers below it.
    #[test]
    fn the_route_purpose_becomes_the_providers_input_type() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);

        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"embeddings":{"float":[[0.25,0.5]]}}"#,
        ));
        let dir = root.join("providers.d");
        std::fs::create_dir_all(&dir).unwrap();
        // A providers.d is 0700; the loader refuses a group/world-writable
        // one, and `create_dir_all` honours the umask.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("cohere.toml");
        std::fs::write(
            &path,
            format!(
                "provider = \"cohere\"\napi_key = \"co-test\"\nbase_url = \"{}\"\n\n\
                 [[models]]\nname = \"cohere-embed-v4-0\"\n\
                 provider_model_id = \"embed-v4.0\"\ndim = 2\n",
                mock.url
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let gateway = Arc::new(Gateway::load(&dir));

        let (server, addr) = spawn_for_test(
            engine,
            &runtime,
            "127.0.0.1:0",
            Duration::from_secs(5),
            gateway,
        )
        .unwrap();
        let client = GrpcClient::new(vec![addr.to_string()], Vec::new(), 5_000, 1_000);

        // What `search()` sends.
        crate::runtime::block_on(client.embed(
            &["a query".to_string()],
            "cohere-embed-v4-0",
            &EmbedRoute::default().with_purpose(crate::client::EmbedPurpose::Query),
        ))
        .expect("query embed");
        assert!(
            mock.last_request()
                .contains("\"input_type\":\"search_query\""),
            "{}",
            mock.last_request()
        );

        // What the worker, one-shot embed() and migrate reembed send — and
        // what a default-constructed route means.
        crate::runtime::block_on(client.embed(
            &["stored content".to_string()],
            "cohere-embed-v4-0",
            &EmbedRoute::default(),
        ))
        .expect("document embed");
        assert!(
            mock.last_request()
                .contains("\"input_type\":\"search_document\""),
            "{}",
            mock.last_request()
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A provider 401 crosses the wire as UPSTREAM_AUTH_FAILED metadata and
    /// classifies Config — bounded retry + failover, never dead-lettering.
    #[test]
    fn provider_auth_failure_crosses_as_upstream_auth_failed() {
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);

        let mock = runtime.block_on(provider_mock::always(401, r#"{"error":"bad key"}"#));
        let gateway = gateway_for(&mock.url);

        let (server, addr) = spawn_for_test(
            engine,
            &runtime,
            "127.0.0.1:0",
            Duration::from_secs(5),
            gateway,
        )
        .unwrap();
        let client = GrpcClient::new(vec![addr.to_string()], Vec::new(), 5_000, 1_000);

        let err = crate::runtime::block_on(client.embed(
            &["hello".to_string()],
            "openai-text-embedding-3-small",
            &EmbedRoute::default(),
        ))
        .unwrap_err();
        match &err {
            PvError::Remote { code, message } => {
                assert_eq!(*code, RavennaCode::UpstreamAuthFailed);
                assert!(
                    !message.contains("sk-test"),
                    "no key in the message: {message}"
                );
            }
            other => panic!("expected Remote(UpstreamAuthFailed), got {other:?}"),
        }
        assert_eq!(err.class(), ErrorClass::Config);

        server.abort();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// §7.3: the provider path takes NO `response_slots` permit (and never
    /// reaches HostPolicy). With a zero-permit semaphore the engine path
    /// could not answer at all — the provider path must.
    #[test]
    fn provider_path_takes_no_response_slot() {
        let root = crate::client::embedded::tests::empty_engine_root();
        let runtime = crate::client::embedded::tests::engine_runtime();
        let engine = crate::client::embedded::tests::test_engine(&root);

        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        let svc = EmbeddedService {
            engine,
            predict_timeout: Duration::from_secs(5),
            // Zero permits: anything that waits on response_slots can never
            // proceed. The provider path must not notice.
            response_slots: Arc::new(Semaphore::new(0)),
            provider_response_slots: Arc::new(Semaphore::new(4)),
            gateway: gateway_for(&mock.url),
        };

        let req = Request::new(EmbedTextsRequest {
            model: "openai-text-embedding-3-small".to_string(),
            texts: vec!["hello".to_string()],
            ..Default::default()
        });
        let response = runtime
            .block_on(svc.embed_texts(req))
            .expect("provider path is not gated by response_slots");
        assert!(response.into_inner().embeddings.is_some());

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod inbound_deadline_tests {
    use super::*;

    fn md(v: &str) -> tonic::metadata::MetadataMap {
        let mut m = tonic::metadata::MetadataMap::new();
        m.insert("grpc-timeout", v.parse().unwrap());
        m
    }

    /// The wire grammar: 1-8 digits plus H/M/S/m/u/n. Absent or malformed
    /// values fall back to the server ceiling (None here).
    #[test]
    fn parses_every_unit_and_rejects_garbage() {
        assert_eq!(inbound_deadline(&md("2S")), Some(Duration::from_secs(2)));
        assert_eq!(
            inbound_deadline(&md("1500m")),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(inbound_deadline(&md("3M")), Some(Duration::from_secs(180)));
        assert_eq!(inbound_deadline(&md("1H")), Some(Duration::from_secs(3600)));
        assert_eq!(
            inbound_deadline(&md("250000u")),
            Some(Duration::from_micros(250000))
        );
        assert_eq!(
            inbound_deadline(&md("999n")),
            Some(Duration::from_nanos(999))
        );
        assert_eq!(inbound_deadline(&md("12")), None, "missing unit");
        assert_eq!(inbound_deadline(&md("xS")), None, "non-numeric");
        assert_eq!(
            inbound_deadline(&md("123456789S")),
            None,
            "gRPC permits at most eight digits"
        );
        assert_eq!(
            inbound_deadline(&tonic::metadata::MetadataMap::new()),
            None,
            "absent header"
        );
    }
}
