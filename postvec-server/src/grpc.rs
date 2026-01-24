//! The gRPC inference service: `EmbedTexts` and `ConvertEmbeddings` over the
//! canonical ninference proto.
//!
//! Lifted from postvec's embedded loopback server
//! (`postvec/src/client/embedded/server.rs`) by way of the packaging test
//! fixture, and deliberately kept as a transcription: the same payload
//! shapes into `predict_raw`, the same envelope guards, the same
//! `x-ravenna-error-code` metadata out. That is what makes a column tested
//! in embedded mode behave identically when its owner moves inference off
//! the database host — every refusal it can meet here, it could already
//! meet there.
//!
//! Two things this service will not do, both on purpose:
//!
//! - **It never loads a model.** An unready name is `MODEL_NOT_LOADED`. A
//!   request-driven load path would let any client on the network expand the
//!   resident set past every ceiling the operator configured.
//! - **It has no transport security and no authentication.** postvec's gRPC
//!   client speaks plaintext and its GUC help says so, so adding TLS on this
//!   side alone would break every existing `postvec setup --grpc`. The port
//!   belongs on a trusted private network.

use crate::metrics::{Metrics, METHOD_CONVERT, METHOD_EMBED};
use crate::proto::ninference_service_server::{NinferenceService, NinferenceServiceServer};
use crate::proto::{
    ConvertEmbeddingsRequest, ConvertEmbeddingsResponse, EmbedTextsRequest, EmbedTextsResponse,
    TokenUsage,
};
use engine::{EngineError, ExecutorOutput, InferenceEngine, InputData};
use prost_types::{ListValue, Value as ProstValue};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
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

struct InferenceService {
    engine: Arc<InferenceEngine>,
    metrics: Arc<Metrics>,
    predict_timeout: Duration,
    /// A completed unary response is encoded lazily after the handler future
    /// returns. This semaphore's permit is moved into the HTTP response body,
    /// so slow/abandoned loopback readers cannot accumulate unbounded prost
    /// response trees after Tower's request-future limit has been released.
    response_slots: Arc<Semaphore>,
}

/// The metric label for a refusal: its wire error code when it carries one,
/// otherwise the gRPC status name. Both are closed sets, so neither can grow
/// the label cardinality of `postvec_server_request_errors_total`.
fn status_label(status: &Status) -> &str {
    status
        .metadata()
        .get("x-ravenna-error-code")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_else(|| match status.code() {
            tonic::Code::InvalidArgument => "INVALID_ARGUMENT",
            tonic::Code::DeadlineExceeded => "DEADLINE_EXCEEDED",
            tonic::Code::ResourceExhausted => "RESOURCE_EXHAUSTED",
            tonic::Code::Unavailable => "UNAVAILABLE",
            tonic::Code::FailedPrecondition => "FAILED_PRECONDITION",
            tonic::Code::NotFound => "NOT_FOUND",
            tonic::Code::Internal => "INTERNAL_ERROR",
            _ => "OTHER",
        })
}

/// The trait impl is a thin instrumentation wrapper so that *every* exit
/// path — including the early refusals before any work happens — releases
/// its in-flight slot and lands in exactly one counter. The handlers
/// themselves are inherent methods below, unchanged from their origin.
#[tonic::async_trait]
impl NinferenceService for InferenceService {
    async fn embed_texts(
        &self,
        request: Request<EmbedTextsRequest>,
    ) -> Result<Response<EmbedTextsResponse>, Status> {
        let items = request.get_ref().texts.len();
        let started = Instant::now();
        self.metrics.request_started(METHOD_EMBED);
        match self.embed_texts_inner(request).await {
            Ok(response) => {
                self.metrics
                    .request_completed(METHOD_EMBED, items, started.elapsed());
                Ok(response)
            }
            Err(status) => {
                self.metrics
                    .request_failed(METHOD_EMBED, status_label(&status));
                Err(status)
            }
        }
    }

    async fn convert_embeddings(
        &self,
        request: Request<ConvertEmbeddingsRequest>,
    ) -> Result<Response<ConvertEmbeddingsResponse>, Status> {
        let items = request.get_ref().embeddings.len();
        let started = Instant::now();
        self.metrics.request_started(METHOD_CONVERT);
        match self.convert_embeddings_inner(request).await {
            Ok(response) => {
                self.metrics
                    .request_completed(METHOD_CONVERT, items, started.elapsed());
                Ok(response)
            }
            Err(status) => {
                self.metrics
                    .request_failed(METHOD_CONVERT, status_label(&status));
                Err(status)
            }
        }
    }
}

impl InferenceService {
    async fn embed_texts_inner(
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
        log::debug!("EmbedTexts model={} texts={}", req.model, req.texts.len());

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
        if !self.engine.is_model_ready(&req.model) {
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
        if req.texts.len() > crate::limits::MAX_REQUEST_ITEMS {
            return Err(Status::invalid_argument(format!(
                "{} texts exceeds the {} items-per-request ceiling; split the request",
                req.texts.len(),
                crate::limits::MAX_REQUEST_ITEMS
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
        let max_items = crate::limits::max_items_for_dim(dim);
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
                .map_err(|_| Status::unavailable("response-capacity gate closed"))?;

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
        if !req.input_type.is_empty() {
            payload.insert("input_type".to_string(), Value::String(req.input_type));
        }
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

    async fn convert_embeddings_inner(
        &self,
        request: Request<ConvertEmbeddingsRequest>,
    ) -> Result<Response<ConvertEmbeddingsResponse>, Status> {
        let deadline_std = request_deadline(&request, self.predict_timeout);
        let deadline = tokio::time::Instant::from_std(deadline_std);
        let req = request.into_inner();
        log::debug!(
            "ConvertEmbeddings model={} embeddings={}",
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
        if req.embeddings.len() > crate::limits::MAX_REQUEST_ITEMS {
            return Err(Status::invalid_argument(format!(
                "{count} embeddings exceeds the {} items-per-request ceiling; split the \
                 request",
                crate::limits::MAX_REQUEST_ITEMS
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
        let max_items = crate::limits::max_items_for_dim(out_dim);
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
                .map_err(|_| Status::unavailable("response-capacity gate closed"))?;

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

/// Bind, then serve until `shutdown` resolves.
///
/// Unlike postvec's embedded loopback server this binds a routable address:
/// the whole point of remote mode is that the engine lives somewhere other
/// than the database host. There is no TLS and no authentication on this
/// port — see the module docs.
///
/// `shutdown` stops the listener accepting new connections; requests already
/// in flight run to completion or to their own deadline, whichever comes
/// first. Callers wire it to SIGTERM/ctrl-c.
pub async fn serve(
    engine: Arc<InferenceEngine>,
    metrics: Arc<Metrics>,
    listener: std::net::TcpListener,
    predict_timeout: Duration,
    max_inflight: usize,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), String> {
    // The socket was reserved before the models loaded, so a port conflict
    // fails the boot in the first second rather than after a long warmup.
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking on the gRPC listener: {e}"))?;
    let incoming: tonic::transport::server::TcpIncoming =
        tokio::net::TcpListener::from_std(listener)
            .map_err(|e| format!("registering the gRPC listener with the reactor: {e}"))?
            .into();
    let bound = incoming
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;
    log::info!("gRPC listening on {bound} (plaintext, unauthenticated — private networks only)");

    let response_slots = Arc::new(Semaphore::new(max_inflight.max(1)));
    let service = NinferenceServiceServer::new(InferenceService {
        engine,
        metrics,
        predict_timeout,
        response_slots,
    })
    .max_encoding_message_size(MAX_ENCODE_MESSAGE_SIZE)
    .max_decoding_message_size(MAX_DECODE_MESSAGE_SIZE);

    // Same layering as the embedded loopback server: the deadline layer sits
    // OUTSIDE the concurrency limit, so time spent queued behind the global
    // cap is charged against the caller's `grpc-timeout`.
    tonic::transport::Server::builder()
        .layer(ResponsePermitLayer { predict_timeout })
        .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
            max_inflight.max(1),
        ))
        .concurrency_limit_per_connection(4)
        .add_service(service)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await
        .map_err(|e| format!("gRPC server exited: {e}"))
}

/// `EngineError` → tonic Status + `x-ravenna-error-code` metadata: the same
/// mapping the production gRPC server applies for engine
/// errors, so postvec's (and any other) client classifies identically.
fn engine_error_to_status(e: EngineError) -> Status {
    let code = e.to_error_code();
    let message = e.to_string();
    log::warn!("request failed [{}]: {message}", code.as_str());
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
        "model {model:?} is not loaded on this node; put it on disk (`postvec model pull \
         {model}`) and load it with `postvec-server load {model}` — inference requests never \
         load models"
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
    if outer.len() > crate::limits::MAX_REQUEST_ITEMS {
        return Err(resource_exhausted_status(format!(
            "executor returned {} embedding rows; ceiling is {}",
            outer.len(),
            crate::limits::MAX_REQUEST_ITEMS
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
             response-tree envelope",
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
            (0..=crate::limits::MAX_REQUEST_ITEMS)
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
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("models").join("onnx-runtime")).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let engine = Arc::new(InferenceEngine::new(Arc::new(engine::EngineConfig {
            root_path: root.path().to_path_buf(),
            host_policy: Default::default(),
        })));
        let metrics = Arc::new(Metrics::new());
        let svc = InferenceService {
            engine,
            metrics: metrics.clone(),
            predict_timeout: Duration::from_secs(30),
            response_slots: Arc::new(Semaphore::new(1)),
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

        // Both refusals went through the instrumentation wrapper, so the
        // in-flight gauge must be back at rest and the failures counted.
        assert_eq!(metrics.in_flight(), 0);
        let rendered = metrics.render(&crate::metrics::Snapshot {
            version: "test",
            features: "onnx".into(),
            start_unix_seconds: 0,
            models_loaded: 0,
            models_enabled_on_disk: 0,
            ready: false,
            draining: false,
            cluster_members: 1,
        });
        assert!(
            rendered.contains("code=\"DEADLINE_EXCEEDED\"} 1"),
            "{rendered}"
        );
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
mod label_tests {
    use super::*;

    /// A refusal that carries the wire code is labelled by it; one that does
    /// not falls back to the gRPC status name. Neither can invent a label.
    #[test]
    fn status_labels_come_from_the_wire_code_then_the_status() {
        assert_eq!(status_label(&invalid_input_status("x")), "INVALID_INPUT");
        assert_eq!(
            status_label(&model_not_loaded_status("m")),
            "MODEL_NOT_LOADED"
        );
        assert_eq!(
            status_label(&Status::deadline_exceeded("no code metadata")),
            "DEADLINE_EXCEEDED"
        );
        assert_eq!(
            status_label(&Status::invalid_argument("no code metadata")),
            "INVALID_ARGUMENT"
        );
        assert_eq!(status_label(&Status::unavailable("gate")), "UNAVAILABLE");
        assert_eq!(status_label(&Status::aborted("odd")), "OTHER");
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
