//! The gRPC inference service: `EmbedTexts` and `ConvertEmbeddings` over the
//! canonical inference proto.
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
use providers::gateway::{Gateway, GatewayError, InputType};
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

/// Aggregate resident ceiling, in MiB, for provider responses that have been
/// built but not yet written. Chosen against the shape of a real answer: the
/// largest batch any supported descriptor can ask for (512 × 3072) encodes to
/// roughly 24 MiB of prost tree, so this holds several concurrent maxima
/// while capping the pathological case — a client that opens calls and never
/// reads them — at a number that fits beside the engine's own envelopes
/// rather than dwarfing them.
const PROVIDER_RESPONSE_BUDGET_MIB: u32 = 256;

/// What a provider response of this shape will occupy once built, in MiB,
/// rounded up and clamped into the budget. The estimate uses the same
/// per-component and per-item constants the envelope check uses, so the
/// permit and the refusal agree about what a response weighs.
fn provider_response_mib(rows: usize, dim: i32) -> u32 {
    let components = (rows as u64).saturating_mul(dim.max(0) as u64);
    let bytes = components
        .saturating_mul(PROST_COMPONENT_BYTES)
        .saturating_add((rows as u64).saturating_mul(TREE_ITEM_OVERHEAD_BYTES));
    let mib = bytes.div_ceil(1024 * 1024).max(1);
    u32::try_from(mib)
        .unwrap_or(PROVIDER_RESPONSE_BUDGET_MIB)
        .min(PROVIDER_RESPONSE_BUDGET_MIB)
}

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
    /// Engine path only. The provider path never takes a slot (see the
    /// dispatch comment in `embed_texts_inner`).
    response_slots: Arc<Semaphore>,
    /// The response-lifetime bound for the provider path, denominated in
    /// **mebibytes of response tree**, not in responses.
    ///
    /// The provider path deliberately skips `response_slots` so a network
    /// call never queues behind CPU-bound ONNX — but "no admission gate" and
    /// "no *response-lifetime* bound" are different things. A completed unary
    /// response is encoded lazily after the handler returns, so the
    /// per-provider semaphore and the tower ingress permit are both released
    /// while the prost tree is still resident, and a slow or abandoned reader
    /// could accumulate them.
    ///
    /// Counting responses does not bound memory. A permit was one response of
    /// *any* size, so a budget of N permits formally allowed N × the 96 MiB
    /// envelope — tens of gigabytes at the configuration ceiling. Weighting by
    /// the size the response actually is makes the number mean what it says:
    /// [`PROVIDER_RESPONSE_BUDGET_MIB`] is the aggregate resident ceiling for
    /// finished-but-unsent provider responses, and one oversized response
    /// costs proportionally.
    ///
    /// It is deliberately **one shared budget** rather than one per provider:
    /// memory is a single resource, and a per-provider budget would multiply
    /// the ceiling by the number of files. The consequence is intended
    /// backpressure — a provider whose callers do not read can make another
    /// provider's callers wait — bounded by every caller's own deadline, and
    /// the same shape the engine path's `response_slots` has always had.
    provider_response_bytes: Arc<Semaphore>,
    /// External-provider gateway. Empty in the zero-config case; `owns()`
    /// decides routing after the engine readiness check.
    gateway: Arc<Gateway>,
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
        // Dispatch: a ready engine model wins its name, then the gateway,
        // then MODEL_NOT_LOADED. Same local-wins rule as `/config`, so
        // discovery and this handler never disagree. Convert requests
        // follow the same order through `owns_converter` (UniVec serves
        // hosted conversion; the other providers embed only).
        //
        // The provider path skips the three max-inflight gates: it took a
        // widened tower slot (see `serve`), it skips `response_slots`, and
        // it never calls `predict_raw_at` (so HostPolicy is not involved).
        // Provider calls are network-bound and must not queue behind
        // CPU-bound ONNX; their limiter is the per-provider
        // `max_concurrent` semaphore inside the gateway.
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
        // Do not forward `input_type` to the engine. The client always
        // sets it (the gateway needs it for Cohere). The engine only
        // applies it to models that declare templates, but a template-less
        // model warns per request, which would flood this node's log, and
        // a templated model's vectors would silently change (the
        // golden-vector suite exists to catch that). Stripping it here
        // keeps engine numbers as they are, with no client-side model
        // lookup. Forwarding it is a separate, tested change.
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

    /// Provider path of `EmbedTexts`: the gateway, bounded by the
    /// per-provider semaphore and the caller's deadline, never by
    /// `response_slots` or HostPolicy (network-bound, must not serialize
    /// behind local ONNX). The response envelope uses the same math as the
    /// engine path, sized from the descriptor's declared dimension.
    async fn embed_via_gateway(
        &self,
        req: EmbedTextsRequest,
        deadline: std::time::Instant,
    ) -> Result<Response<EmbedTextsResponse>, Status> {
        if req.texts.len() > crate::limits::MAX_REQUEST_ITEMS {
            return Err(Status::invalid_argument(format!(
                "{} texts exceeds the {} items-per-request ceiling; split the request",
                req.texts.len(),
                crate::limits::MAX_REQUEST_ITEMS
            )));
        }
        // The gateway validates every returned vector against this dim, so
        // the envelope math is exact, not best-effort.
        let dim = self
            .gateway
            .dim(&req.model)
            .map(|d| d as i32)
            .unwrap_or(16_000);
        let max_items = crate::limits::max_items_for_dim(dim);
        if req.texts.len() > max_items {
            return Err(Status::invalid_argument(format!(
                "{} texts at {dim} dims exceeds the response envelope; send at most \
                 {max_items} per request",
                req.texts.len()
            )));
        }

        // Taken BEFORE the call, so no paid embedding is ever thrown away for
        // want of capacity, and released only when the encoded body is
        // dropped (see `ResponsePermit` below). Weighted by what this
        // response will actually occupy, and bounded by the caller's deadline
        // like every other wait on this path.
        let weight = provider_response_mib(req.texts.len(), dim);
        let response_permit = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.provider_response_bytes
                .clone()
                .acquire_many_owned(weight),
        )
        .await
        .map_err(|_| {
            Status::deadline_exceeded(format!(
                "deadline exhausted waiting for {weight} MiB of provider response capacity \
                 (aggregate budget {PROVIDER_RESPONSE_BUDGET_MIB} MiB)"
            ))
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
        // The permit rides the response body: `ResponsePermitLayer` moves
        // it into `PermitBody`, which holds it until hyper finishes or
        // drops the encoding. A completed tree is not done until the
        // bytes leave.
        response
            .extensions_mut()
            .insert(ResponsePermit(Arc::new(response_permit)));
        Ok(response)
    }

    /// Provider path of `ConvertEmbeddings`: same shape as
    /// [`Self::embed_via_gateway`]. Bounded by the per-provider semaphore
    /// and the caller's deadline, never by `response_slots` or HostPolicy.
    /// The byte-weighted response permit is taken before the call so a paid
    /// conversion is never thrown away for want of capacity.
    async fn convert_via_gateway(
        &self,
        req: ConvertEmbeddingsRequest,
        deadline: std::time::Instant,
    ) -> Result<Response<ConvertEmbeddingsResponse>, Status> {
        // The gateway serves direct conversion only; the bridge fields
        // belong to engine executors. Refusing them beats ignoring them —
        // a call that names a chain must not silently get a single hop.
        if !(req.source_model.is_empty()
            && req.bridge_model.is_empty()
            && req.target_model.is_empty())
        {
            return Err(Status::invalid_argument(
                "bridge fields (source_model/bridge_model/target_model) are not served by a \
                 provider-backed converter; call it by its name alone",
            ));
        }
        if req.embeddings.len() > crate::limits::MAX_REQUEST_ITEMS {
            return Err(Status::invalid_argument(format!(
                "{} embeddings exceeds the {} items-per-request ceiling; split the request",
                req.embeddings.len(),
                crate::limits::MAX_REQUEST_ITEMS
            )));
        }
        // The gateway validates every returned vector against this dim, so
        // the envelope math is exact, not best-effort.
        let dim = self
            .gateway
            .dim(&req.model)
            .map(|d| d as i32)
            .unwrap_or(16_000);
        let max_items = crate::limits::max_items_for_dim(dim);
        if req.embeddings.len() > max_items {
            return Err(Status::invalid_argument(format!(
                "{} embeddings at a {dim}-dim target exceeds the response envelope; send at \
                 most {max_items} per request",
                req.embeddings.len()
            )));
        }

        let weight = provider_response_mib(req.embeddings.len(), dim);
        let response_permit = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.provider_response_bytes
                .clone()
                .acquire_many_owned(weight),
        )
        .await
        .map_err(|_| {
            Status::deadline_exceeded(format!(
                "deadline exhausted waiting for {weight} MiB of provider response capacity \
                 (aggregate budget {PROVIDER_RESPONSE_BUDGET_MIB} MiB)"
            ))
        })?
        .map_err(|_| Status::unavailable("provider response-capacity gate closed"))?;

        // Width and finiteness of every input are the gateway's checks —
        // made before any paid call, against the descriptor's source_dim.
        let inputs: Vec<Vec<f32>> = req.embeddings.into_iter().map(|fv| fv.vector).collect();
        let vectors = self
            .gateway
            .convert(&req.model, &inputs, deadline)
            .await
            .map_err(gateway_error_to_status)?;

        let mut response = Response::new(ConvertEmbeddingsResponse {
            embeddings: Some(vectors_to_list_value(vectors, deadline)?),
            usage: None,
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
            // Same dispatch rule as EmbedTexts (see the comment there), with
            // `owns_converter`: a provider-backed *embed* model cannot
            // convert, and MODEL_NOT_LOADED for it is the same answer the
            // engine gives every name it does not hold.
            if !self.engine.is_model_loaded(&req.model) && self.gateway.owns_converter(&req.model) {
                return self.convert_via_gateway(req, deadline_std).await;
            }
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
    gateway: Arc<Gateway>,
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
    // Tower ingress stays the decode-amplification backstop, widened by
    // the sum of per-provider `max_concurrent` so network-bound provider
    // calls never queue behind CPU-bound ONNX. The budget is the gateway
    // loaded at boot; a later `/admin/providers/reload` that raises it
    // still shares this width until the next restart (serving is correct,
    // admission is tighter; the reload route reports restart_needed).
    // Zero-config: budget 0, size unchanged. Residual: a burst of engine
    // EmbedTexts can occupy the extra tower slots, decode, then wait on
    // `response_slots`. Extra decoded RSS exists only when providers are
    // configured; callers still carry `grpc-timeout`.
    let ingress_limit = max_inflight.max(1) + gateway.inflight_budget();
    // The response-lifetime bound for the provider path, in MiB of response
    // tree. Fixed rather than derived from the provider count: it is an
    // aggregate memory ceiling, and memory does not grow because a second
    // connector file appeared.
    let provider_response_bytes = Arc::new(Semaphore::new(PROVIDER_RESPONSE_BUDGET_MIB as usize));
    let service = NinferenceServiceServer::new(InferenceService {
        engine,
        metrics,
        predict_timeout,
        response_slots,
        provider_response_bytes,
        gateway,
    })
    .max_encoding_message_size(MAX_ENCODE_MESSAGE_SIZE)
    .max_decoding_message_size(MAX_DECODE_MESSAGE_SIZE);

    // Same layering as the embedded loopback server: the deadline layer sits
    // OUTSIDE the concurrency limit, so time spent queued behind the global
    // cap is charged against the caller's `grpc-timeout`.
    tonic::transport::Server::builder()
        .layer(ResponsePermitLayer { predict_timeout })
        .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
            ingress_limit,
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

/// [`GatewayError`] to tonic Status + `x-ravenna-error-code`, the same
/// wire contract as [`engine_error_to_status`]. The client classifies
/// provider failures like this node's engine answers. Messages are
/// secret-free by construction in the providers crate.
fn gateway_error_to_status(e: GatewayError) -> Status {
    let code = e.code;
    let message = e.message;
    // The code and the provider name, never the message. The message can
    // carry a bounded slice of a provider's error body, and a provider can
    // echo the *source text* it was asked to embed — which in remote mode
    // would put a fragment of a database's content into a different
    // machine's log. It still travels to the caller, over the wire, to the
    // database the text came from: that is the one place it is not a
    // disclosure, and the one place it is useful.
    log::warn!("provider request failed [{}]", code.as_str());
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
             response-tree envelope",
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

    /// Counting responses does not bound memory: a permit used to be one
    /// response of any size, so N permits formally allowed N × the 96 MiB
    /// envelope. The weight has to track the shape.
    #[test]
    fn the_provider_response_weight_tracks_the_response_size() {
        // A small answer costs the 1 MiB floor.
        assert_eq!(provider_response_mib(1, 2), 1);
        // The largest batch any supported descriptor can ask for is a
        // meaningful fraction of the budget, not one slot of it.
        let biggest = provider_response_mib(512, 3072);
        assert!(
            biggest > 8,
            "512 x 3072 must cost real budget, got {biggest}"
        );
        assert!(biggest <= PROVIDER_RESPONSE_BUDGET_MIB);
        // Bigger responses cost strictly more.
        assert!(provider_response_mib(512, 3072) > provider_response_mib(64, 3072));
        // Absurd shapes clamp instead of overflowing or panicking on the
        // u32 conversion `acquire_many_owned` needs.
        assert_eq!(
            provider_response_mib(usize::MAX, i32::MAX),
            PROVIDER_RESPONSE_BUDGET_MIB
        );
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
            // A deliberately tiny budget: one MiB, which is what a one-row,
            // two-component response weighs after rounding up.
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
            provider_response_bytes: Arc::new(Semaphore::new(
                PROVIDER_RESPONSE_BUDGET_MIB as usize,
            )),
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
mod gateway_tests {
    use super::*;
    use providers::testing as provider_mock;

    fn test_engine(root: &std::path::Path) -> Arc<InferenceEngine> {
        std::fs::create_dir_all(root.join("models").join("onnx-runtime")).unwrap();
        Arc::new(InferenceEngine::new(Arc::new(engine::EngineConfig {
            root_path: root.to_path_buf(),
            host_policy: Default::default(),
        })))
    }

    /// A providers.d directory serving one 2-dim OpenAI-typed model pointed
    /// at `base_url`, loaded into a gateway.
    fn gateway_for(dir: &std::path::Path, base_url: &str) -> Arc<Gateway> {
        use std::os::unix::fs::PermissionsExt;
        let providers_dir = dir.join("providers.d");
        std::fs::create_dir_all(&providers_dir).unwrap();
        // A providers.d is 0700; the loader refuses a group/world-writable
        // one, and `create_dir_all` honours the umask.
        std::fs::set_permissions(&providers_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // …and its parent: the loader refuses a providers.d whose ancestor is
        // group-writable, and `tempfile`/`create_dir_all` honour the umask.
        if let Some(parent) = &providers_dir.parent() {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = providers_dir.join("openai.toml");
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
        Arc::new(Gateway::load(&providers_dir, &Default::default()))
    }

    fn service_with(
        engine: Arc<InferenceEngine>,
        gateway: Arc<Gateway>,
        provider_response_bytes: Arc<Semaphore>,
    ) -> InferenceService {
        InferenceService {
            engine,
            metrics: Arc::new(Metrics::new()),
            predict_timeout: Duration::from_secs(5),
            // Zero permits: anything that waits on response_slots can never
            // proceed — which is exactly what the provider path must not do.
            response_slots: Arc::new(Semaphore::new(0)),
            provider_response_bytes,
            gateway,
        }
    }

    /// The ordinary shape: a provider response budget wide enough not to be
    /// the thing under test.
    fn service(engine: Arc<InferenceEngine>, gateway: Arc<Gateway>) -> InferenceService {
        service_with(
            engine,
            gateway,
            Arc::new(Semaphore::new(PROVIDER_RESPONSE_BUDGET_MIB as usize)),
        )
    }

    /// A completed unary response is encoded *lazily*, after the handler
    /// future has returned — so by the time the bytes are written, the
    /// per-provider semaphore and the tower ingress permit are both long
    /// released. Without a response-lifetime permit, a slow or abandoned
    /// reader could accumulate finished provider trees without limit; in
    /// embedded mode that is the PostgreSQL launcher's RSS.
    ///
    /// The engine path has held such a permit since it was written. This is
    /// the same mechanism on its own budget, and the assertion is the one
    /// that matters: the permit is still held when the handler has returned.
    #[test]
    fn a_provider_response_holds_its_permit_until_the_body_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        let gateway = gateway_for(root.path(), &mock.url);
        let slots = Arc::new(Semaphore::new(1));
        let svc = service_with(test_engine(root.path()), gateway, slots.clone());

        let response = runtime
            .block_on(svc.embed_texts(Request::new(EmbedTextsRequest {
                model: "openai-text-embedding-3-small".to_string(),
                texts: vec!["hello".to_string()],
                ..Default::default()
            })))
            .expect("provider embed");

        // The handler has returned and the vectors are in hand — and the
        // budget is still spent, because the response has not been written.
        // The permit is weighted: this response's own MiB, not "one response".
        assert_eq!(
            slots.available_permits(),
            0,
            "a finished-but-unsent provider response must still hold its permit"
        );
        assert!(
            response.extensions().get::<ResponsePermit>().is_some(),
            "the permit must ride the response for ResponsePermitLayer to move \
             into the body"
        );
        drop(response);
        assert_eq!(
            slots.available_permits(),
            1,
            "dropping it releases the budget"
        );
    }

    /// A provider-only name serves (the gateway is consulted before
    /// MODEL_NOT_LOADED) and takes no `response_slots` permit; convert with
    /// the same name never touches the gateway.
    #[test]
    fn provider_names_serve_without_engine_gates_and_convert_never_routes() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        let svc = service(
            test_engine(root.path()),
            gateway_for(root.path(), &mock.url),
        );

        let req = Request::new(EmbedTextsRequest {
            model: "openai-text-embedding-3-small".to_string(),
            texts: vec!["hello".to_string()],
            ..Default::default()
        });
        let response = runtime
            .block_on(svc.embed_texts(req))
            .expect("provider path is not gated by response_slots");
        assert!(response.into_inner().embeddings.is_some());

        // A convert request consults the gateway only through
        // `owns_converter`: a provider-backed EMBED model refuses exactly
        // like any model the engine does not hold — only a `kind =
        // "convert"` entry dispatches (see the univec tests below).
        let req = Request::new(ConvertEmbeddingsRequest {
            model: "openai-text-embedding-3-small".to_string(),
            ..Default::default()
        });
        let err = runtime.block_on(svc.convert_embeddings(req)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            err.metadata().get("x-ravenna-error-code").unwrap(),
            "MODEL_NOT_LOADED"
        );
        assert_eq!(
            mock.request_count(),
            1,
            "only the embed dialed the provider"
        );
    }

    /// A UniVec providers.d beside the engine: one embed model and one
    /// converter (`source_dim` 3 → `dim` 2, distinct on purpose so a
    /// swapped-axis bug cannot pass both width checks).
    fn univec_gateway_for(dir: &std::path::Path, base_url: &str) -> Arc<Gateway> {
        use std::os::unix::fs::PermissionsExt;
        let providers_dir = dir.join("providers.d");
        std::fs::create_dir_all(&providers_dir).unwrap();
        std::fs::set_permissions(&providers_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if let Some(parent) = &providers_dir.parent() {
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = providers_dir.join("univec.toml");
        std::fs::write(
            &path,
            format!(
                "provider = \"univec\"\napi_key = \"uv-test\"\nbase_url = \"{base_url}\"\n\n\
                 [[models]]\nname = \"univec-arctic\"\n\
                 provider_model_id = \"snowflake-arctic-embed-l-v2.0\"\ndim = 2\n\n\
                 [[models]]\nname = \"univec-convert-a-to-b\"\nkind = \"convert\"\n\
                 provider_model_id = \"target-space\"\nprovider_source_id = \"source-space\"\n\
                 source_model = \"model-a\"\ntarget_model = \"model-b\"\n\
                 source_dim = 3\ndim = 2\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        Arc::new(Gateway::load(&providers_dir, &Default::default()))
    }

    /// The convert twin of the provider dispatch proof: a `kind = "convert"`
    /// name serves `ConvertEmbeddings` through the gateway — bypassing
    /// `response_slots` (zero permits here) — and its byte-weighted response
    /// permit is still held when the handler has returned, exactly like the
    /// embed path's.
    #[test]
    fn a_provider_converter_serves_convert_and_holds_its_permit() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"success":true,"data":{"embeddings":[[0.25,0.5]]}}"#,
        ));
        let gateway = univec_gateway_for(root.path(), &mock.url);
        let slots = Arc::new(Semaphore::new(1));
        let svc = service_with(test_engine(root.path()), gateway, slots.clone());

        let response = runtime
            .block_on(
                svc.convert_embeddings(Request::new(ConvertEmbeddingsRequest {
                    model: "univec-convert-a-to-b".to_string(),
                    embeddings: vec![crate::proto::FloatVector {
                        vector: vec![0.1, 0.2, 0.3],
                    }],
                    ..Default::default()
                })),
            )
            .expect("provider convert");

        assert_eq!(
            slots.available_permits(),
            0,
            "a finished-but-unsent provider conversion must still hold its permit"
        );
        assert!(
            response.extensions().get::<ResponsePermit>().is_some(),
            "the permit must ride the response for ResponsePermitLayer to move into the body"
        );
        assert!(response.get_ref().embeddings.is_some());
        drop(response);
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(mock.request_count(), 1);
        // The provider-side pair went out on the wire; the postvec-side
        // names stayed home.
        let body = mock.last_request();
        assert!(body.contains("\"source_model\":\"source-space\""), "{body}");
        assert!(body.contains("\"target_model\":\"target-space\""), "{body}");
    }

    /// The gateway serves direct conversion only: a request that names a
    /// bridge chain must be refused, not silently served a single hop.
    #[test]
    fn a_gateway_convert_with_bridge_fields_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"success":true,"data":{"embeddings":[[0.25,0.5]]}}"#,
        ));
        let svc = service(
            test_engine(root.path()),
            univec_gateway_for(root.path(), &mock.url),
        );

        let err = runtime
            .block_on(
                svc.convert_embeddings(Request::new(ConvertEmbeddingsRequest {
                    model: "univec-convert-a-to-b".to_string(),
                    embeddings: vec![crate::proto::FloatVector {
                        vector: vec![0.1, 0.2, 0.3],
                    }],
                    bridge_model: "some-bridge".to_string(),
                    ..Default::default()
                })),
            )
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(mock.request_count(), 0, "no paid call was spent");
    }

    /// Auth mapping on the wire matches the embedded host: 401 ->
    /// Unauthenticated + UPSTREAM_AUTH_FAILED.
    #[test]
    fn provider_auth_failures_carry_the_wire_code() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let mock = runtime.block_on(provider_mock::always(401, r#"{"error":"bad key"}"#));
        let svc = service(
            test_engine(root.path()),
            gateway_for(root.path(), &mock.url),
        );

        let req = Request::new(EmbedTextsRequest {
            model: "openai-text-embedding-3-small".to_string(),
            texts: vec!["hello".to_string()],
            ..Default::default()
        });
        let err = runtime.block_on(svc.embed_texts(req)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unauthenticated);
        assert_eq!(
            err.metadata().get("x-ravenna-error-code").unwrap(),
            "UPSTREAM_AUTH_FAILED"
        );
        assert!(!err.message().contains("sk-test"), "{}", err.message());
    }

    /// A loaded local model wins its name on the embed path; the provider
    /// claiming it is never dialed. The fixture is a dummy-executor model,
    /// whose error proves which path served.
    #[test]
    fn a_loaded_local_model_wins_the_name_collision() {
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let name = "openai-text-embedding-3-small";
        let dir = root.path().join("models").join("generic").join(name);
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
        let engine = test_engine(root.path());
        runtime.block_on(engine.load_model(name)).unwrap();
        assert!(engine.is_model_ready(name), "local fixture is resident");

        let mock = runtime.block_on(provider_mock::always(
            200,
            r#"{"data":[{"embedding":[0.25,0.5],"index":0}]}"#,
        ));
        let gateway = gateway_for(root.path(), &mock.url);
        assert!(gateway.owns(name), "the provider file claims the name");

        let svc = InferenceService {
            engine,
            metrics: Arc::new(Metrics::new()),
            predict_timeout: Duration::from_secs(5),
            response_slots: Arc::new(Semaphore::new(1)),
            provider_response_bytes: Arc::new(Semaphore::new(
                PROVIDER_RESPONSE_BUDGET_MIB as usize,
            )),
            gateway,
        };
        let req = Request::new(EmbedTextsRequest {
            model: name.to_string(),
            texts: vec!["hello".to_string()],
            ..Default::default()
        });
        let err = runtime.block_on(svc.embed_texts(req)).unwrap_err();
        assert!(
            err.message().contains("dummy executor"),
            "the engine served the name: {}",
            err.message()
        );
        assert_eq!(
            mock.request_count(),
            0,
            "the provider must never be dialed for a local-won name"
        );
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
