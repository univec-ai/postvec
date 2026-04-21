//! gRPC transport to running inference nodes, plus `GET /config` over
//! HTTP for model discovery.

use super::{EmbedRoute, InferenceClient, ModelInfo, PvError, RavennaCode};
use crate::proto::ninference_service_client::NinferenceServiceClient;
use crate::proto::{ConvertEmbeddingsRequest, EmbedTextsRequest, FloatVector};
use once_cell::sync::Lazy;
use prost_types::ListValue;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tonic::transport::{Channel, Endpoint};

/// Large embedding batches can exceed tonic's 4 MB default. A much larger
/// blanket decode limit would let a buggy or hostile peer drive GB-scale
/// amplification (a protobuf `Value` tree of doubles expands severalfold
/// while decoding). Requests are bounded upstream by
/// `postvec.max_batch_total_bytes` (16 MiB default) and every embed/convert
/// call is sub-batched by the target dimension to a <= 48 MiB expected
/// response (`jobs::max_items_for_dim`), so 64 MiB in each direction is
/// headroom, not a scheduler.
const MAX_ENCODE_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
const MAX_DECODE_MESSAGE_SIZE: usize = 64 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Process-global channel cache keyed by `host:port`. Channels are cheap to
/// clone; caching survives per-call client construction so repeated calls
/// (worker loop, per-statement search) reuse connections.
static CHANNELS: Lazy<Mutex<HashMap<String, Channel>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static ROUND_ROBIN: AtomicUsize = AtomicUsize::new(0);

/// Peers whose last RPC hit the caller's deadline (an established connection
/// that hangs skips the connect budget entirely). A suspect's next attempt
/// is capped at HALF the remaining budget while other nodes are untried, so
/// one hung-but-connected peer cannot repeatedly consume a short query
/// budget with no failover. Entries expire after [`SUSPECT_TTL`]; a healthy
/// completed RPC clears the mark.
static SUSPECTS: Lazy<Mutex<HashMap<String, std::time::Instant>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
const SUSPECT_TTL: Duration = Duration::from_secs(30);

fn mark_suspect(addr: &str) {
    let mut m = SUSPECTS.lock().unwrap_or_else(|p| p.into_inner());
    m.insert(addr.to_string(), std::time::Instant::now());
}

fn clear_suspect(addr: &str) {
    let mut m = SUSPECTS.lock().unwrap_or_else(|p| p.into_inner());
    m.remove(addr);
}

fn is_suspect(addr: &str) -> bool {
    let mut m = SUSPECTS.lock().unwrap_or_else(|p| p.into_inner());
    match m.get(addr) {
        Some(at) if at.elapsed() < SUSPECT_TTL => true,
        Some(_) => {
            m.remove(addr);
            false
        }
        None => false,
    }
}

fn cached_channel(addr: &str) -> Option<Channel> {
    // A poisoned cache mutex must not panic every later call in the process:
    // the critical sections are simple map ops, so the data is always
    // consistent — recover the guard and carry on.
    let cache = CHANNELS.lock().unwrap_or_else(|p| p.into_inner());
    cache.get(addr).cloned()
}

fn parse_endpoint(addr: &str) -> Result<Endpoint, PvError> {
    Ok(Endpoint::from_shared(format!("http://{addr}"))
        .map_err(|e| PvError::Transport {
            endpoint: addr.to_string(),
            message: format!("invalid endpoint: {e}"),
        })?
        .connect_timeout(CONNECT_TIMEOUT))
}

/// A ready channel for `addr`: the cached connection when one exists,
/// otherwise an eager connect bounded by `connect_budget`. Separating
/// connect from the RPC keeps failover (dead or blackholed nodes) from
/// eating the caller's inference deadline. A failed/slow connect costs at
/// most `connect_budget`; a healthy node then runs its RPC with the whole
/// remaining request budget intact.
async fn connected_channel(addr: &str, connect_budget: Duration) -> Result<Channel, PvError> {
    if let Some(ch) = cached_channel(addr) {
        return Ok(ch);
    }
    let endpoint = parse_endpoint(addr)?;
    match tokio::time::timeout(connect_budget, endpoint.connect()).await {
        Ok(Ok(ch)) => {
            let mut cache = CHANNELS.lock().unwrap_or_else(|p| p.into_inner());
            cache.insert(addr.to_string(), ch.clone());
            Ok(ch)
        }
        Ok(Err(e)) => Err(PvError::Transport {
            endpoint: addr.to_string(),
            message: format!("connect: {e}"),
        }),
        Err(_) => Err(PvError::Transport {
            endpoint: addr.to_string(),
            message: format!("connect timed out after {connect_budget:?}"),
        }),
    }
}

/// Test-only cache seeding (production channels are cached by
/// [`connected_channel`] after a successful eager connect).
#[cfg(test)]
fn cache_lazy_channel(addr: &str) -> Result<(), PvError> {
    let endpoint = parse_endpoint(addr)?;
    let ch = endpoint.connect_lazy();
    let mut cache = CHANNELS.lock().unwrap_or_else(|p| p.into_inner());
    cache.insert(addr.to_string(), ch);
    Ok(())
}

/// Drop a cached channel (after a transport failure) so the next call
/// reconnects from scratch.
fn evict_channel(addr: &str) {
    let mut cache = CHANNELS.lock().unwrap_or_else(|p| p.into_inner());
    cache.remove(addr);
}

/// Drop cached channels for endpoints no longer configured, so topology churn
/// (SIGHUP endpoint changes) does not accumulate dead connections in a
/// long-lived worker.
fn prune_channels(valid: &[String]) {
    let mut cache = CHANNELS.lock().unwrap_or_else(|p| p.into_inner());
    cache.retain(|addr, _| valid.iter().any(|v| v == addr));
    // Suspect marks follow the same topology: without this, a SIGHUP
    // endpoint change strands stale suspect entries in a long-lived
    // launcher (bounded only in practice, not in principle).
    let mut suspects = SUSPECTS.lock().unwrap_or_else(|p| p.into_inner());
    suspects.retain(|addr, _| valid.iter().any(|v| v == addr));
}

/// Warn when endpoint GUC entries were dropped by validation — once per
/// distinct rejected set per GUC, so hot construction paths (per-statement
/// search builds a client) cannot spam the log, but a config change that
/// silently loses an endpoint is still operator-visible.
fn warn_rejected_endpoints(guc_name: &str, rejected: &[String]) {
    if rejected.is_empty() {
        return;
    }
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rejected.hash(&mut hasher);
    let fingerprint = hasher.finish();
    static LAST_WARNED: Lazy<Mutex<HashMap<String, u64>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    let mut last = LAST_WARNED.lock().unwrap_or_else(|p| p.into_inner());
    if last.insert(guc_name.to_string(), fingerprint) == Some(fingerprint) {
        return;
    }
    pgrx::warning!(
        "postvec: {} entries in {guc_name} were dropped by validation (overlong, or past the \
         {}-entry cap): {:?}",
        rejected.len(),
        crate::gucs::MAX_LIST_ENTRIES,
        rejected
    );
}

fn model_config_error_should_failover(err: &PvError) -> bool {
    matches!(
        err,
        PvError::Remote {
            code: RavennaCode::ModelNotFound
                | RavennaCode::ModelNotLoaded
                | RavennaCode::ModelDisabled
                | RavennaCode::BridgePathNotFound
                | RavennaCode::ConverterNotFound
                // Provider auth is per node (providers.d lives on each
                // inference host): in a fleet another node may hold a valid
                // key, so a 401/403 is worth one pass over the other nodes.
                | RavennaCode::UpstreamAuthFailed,
            ..
        }
    )
}

pub struct GrpcClient {
    grpc_endpoints: Vec<String>,
    http_endpoints: Vec<String>,
    /// Per-RPC deadline (worker: postvec.embed_timeout_ms; backends:
    /// postvec.query_timeout_ms).
    rpc_timeout: Duration,
    /// Per-node HTTP timeout for `GET /config` (postvec.discovery_timeout_ms).
    discovery_timeout: Duration,
}

impl GrpcClient {
    pub fn new(
        grpc_endpoints: Vec<String>,
        http_endpoints: Vec<String>,
        rpc_timeout_ms: u64,
        discovery_timeout_ms: u64,
    ) -> Self {
        // Deduplicate (a repeated endpoint would burn failover budget on the
        // same node twice) and cap the fan-out: an unbounded endpoint list is
        // an unbounded discovery/failover multiplier. First occurrence wins,
        // order preserved.
        let dedup_cap = |list: Vec<String>| {
            let mut out: Vec<String> = Vec::new();
            for e in list {
                if !out.contains(&e) && out.len() < crate::gucs::MAX_LIST_ENTRIES {
                    out.push(e);
                }
            }
            out
        };
        Self {
            grpc_endpoints: dedup_cap(grpc_endpoints),
            http_endpoints: dedup_cap(http_endpoints),
            rpc_timeout: Duration::from_millis(rpc_timeout_ms),
            discovery_timeout: Duration::from_millis(discovery_timeout_ms),
        }
    }

    /// Construct from the current GUC values. In embedded mode
    /// (`postvec.mode = 'embedded'`) every gRPC call goes to the engine
    /// host's loopback server instead of the mesh endpoints, and HTTP
    /// discovery goes to its loopback `/config` listener — this is what
    /// keeps connection backends (and, in launcher mode, the per-database
    /// workers) byte-identical to the remote deployment shape.
    pub fn from_gucs(rpc_timeout_ms: u64) -> Self {
        let mode = crate::gucs::mode();
        // Endpoint GUCs ride the same validated parse as postvec.database
        // (URL-sized length cap): overlong entries and everything past the
        // 16-entry ceiling are dropped at parse, so hot paths never see the
        // raw list. Drops are warned (once per distinct rejected set) so a
        // silently ignored endpoint is operator-visible, like the database
        // list's handling.
        let (grpc_ok, grpc_rejected) =
            crate::gucs::parse_validated_list(crate::gucs::GRPC_ENDPOINTS.get(), 512);
        warn_rejected_endpoints("postvec.grpc_endpoints", &grpc_rejected);
        let grpc = resolve_grpc_endpoints(mode, &crate::gucs::embedded_listen(), grpc_ok);
        prune_channels(&grpc);
        let (http_ok, http_rejected) =
            crate::gucs::parse_validated_list(crate::gucs::HTTP_ENDPOINTS.get(), 512);
        warn_rejected_endpoints("postvec.http_endpoints", &http_rejected);
        let http = resolve_http_endpoints(mode, &crate::gucs::embedded_http_listen(), http_ok);
        Self::new(
            grpc,
            http,
            rpc_timeout_ms,
            crate::gucs::DISCOVERY_TIMEOUT_MS.get().max(100) as u64,
        )
    }

    fn client_on(channel: Channel) -> NinferenceServiceClient<Channel> {
        NinferenceServiceClient::new(channel)
            .max_encoding_message_size(MAX_ENCODE_MESSAGE_SIZE)
            .max_decoding_message_size(MAX_DECODE_MESSAGE_SIZE)
    }

    /// Overall caller-side budget for one logical gRPC request. The failover
    /// loop shares ONE total deadline (`rpc_timeout`) across all endpoints —
    /// it does not grant each endpoint the full timeout, so the wall-clock
    /// bound no longer multiplies with the endpoint count. The extra second
    /// covers channel setup around the loop.
    pub fn overall_timeout_ms(&self) -> u64 {
        (self.rpc_timeout.as_millis() as u64).saturating_add(1_000)
    }

    /// Round-robin over endpoints with failover under one total deadline,
    /// with connection establishment budgeted SEPARATELY from inference:
    /// the connect phase for a not-yet-cached node gets at most
    /// `CONNECT_TIMEOUT` — and never more than half the remaining budget
    /// while other nodes are untried — so a dead or blackholed leading
    /// endpoint fails over cheaply; once connected, the RPC gets the FULL
    /// remaining budget, so a healthy (even slow-but-valid) node's inference
    /// deadline does not shrink as endpoints are added. A node that hangs
    /// *after* connecting consumes the remaining budget — the accepted
    /// trade. Transient transport errors move
    /// on to the next endpoint; anything else returns immediately. A pending
    /// PostgreSQL interrupt (query cancel / termination) stops the loop
    /// between attempts so the caller's next SPI call can service it.
    async fn with_failover<T, F, Fut>(&self, mut call: F) -> Result<T, PvError>
    where
        F: FnMut(NinferenceServiceClient<Channel>, Duration) -> Fut,
        Fut: std::future::Future<Output = Result<T, tonic::Status>>,
    {
        if self.grpc_endpoints.is_empty() {
            return Err(PvError::NoEndpoints);
        }
        let n = self.grpc_endpoints.len();
        let start = ROUND_ROBIN.fetch_add(1, Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + self.rpc_timeout;
        let mut last_err = PvError::NoEndpoints;
        for i in 0..n {
            if i > 0 && interrupt_pending() {
                return Err(PvError::Transport {
                    endpoint: "-".into(),
                    message: "PostgreSQL interrupt pending; failover abandoned".into(),
                });
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(PvError::Deadline {
                    ms: self.rpc_timeout.as_millis() as u64,
                });
            }
            let addr = &self.grpc_endpoints[(start + i) % n];
            // CONNECT PHASE. A malformed entry, refused/dead node, or
            // blackholed node must not abort the whole call (or eat the
            // whole budget) while healthy nodes remain — record the error
            // and fail over. Never spend more than half the remaining
            // budget establishing a connection while other nodes are still
            // untried.
            let connect_budget = if i + 1 < n {
                CONNECT_TIMEOUT.min(remaining / 2)
            } else {
                CONNECT_TIMEOUT.min(remaining)
            };
            let channel = match connected_channel(addr, connect_budget).await {
                Ok(channel) => channel,
                Err(e) => {
                    last_err = e;
                    continue;
                }
            };
            // RPC PHASE: the full remaining budget — halved only for a peer
            // whose LAST call hung to its deadline, while other nodes remain
            // untried (a cached hung connection skips the connect budget, so
            // this is the one lever against it; healthy warm peers keep the
            // whole deadline).
            let mut attempt_timeout =
                deadline.saturating_duration_since(tokio::time::Instant::now());
            if attempt_timeout.is_zero() {
                return Err(PvError::Deadline {
                    ms: self.rpc_timeout.as_millis() as u64,
                });
            }
            if i + 1 < n && is_suspect(addr) {
                attempt_timeout /= 2;
            }
            // The attempt budget travels with the request: the closure wraps
            // its message in a `tonic::Request` and calls `set_timeout`, so
            // the server receives a `grpc-timeout` header and can bound its
            // native work to the caller's actual deadline. Passing a raw
            // protobuf message would send no header, and the loopback
            // server's inbound-deadline clamp would never engage.
            let fut = call(Self::client_on(channel), attempt_timeout);
            match tokio::time::timeout(attempt_timeout, fut).await {
                Ok(Ok(v)) => {
                    clear_suspect(addr);
                    return Ok(v);
                }
                Ok(Err(status)) => {
                    let err = status_to_error(addr, &status);
                    match &err {
                        PvError::Transport { .. } => {
                            evict_channel(addr);
                            last_err = err; // try the next node
                        }
                        _ if model_config_error_should_failover(&err) && i + 1 < n => {
                            last_err = err; // another node may have this model loaded
                        }
                        _ => return Err(err),
                    }
                }
                Err(_) => {
                    evict_channel(addr);
                    mark_suspect(addr);
                    last_err = PvError::Deadline {
                        ms: attempt_timeout.as_millis() as u64,
                    };
                }
            }
        }
        Err(last_err)
    }
}

/// Read-only peek at PostgreSQL's pending-interrupt flag. Deliberately never
/// calls `CHECK_FOR_INTERRUPTS()` here: that can `ereport`/longjmp, which
/// must not unwind through the tokio runtime — the caller's next SPI call is
/// where the interrupt is actually serviced.
#[cfg(not(test))]
fn interrupt_pending() -> bool {
    unsafe { pgrx::pg_sys::InterruptPending != 0 }
}

/// The unit-test binary runs outside PostgreSQL, where the flag's storage
/// does not exist.
#[cfg(test)]
fn interrupt_pending() -> bool {
    false
}

impl InferenceClient for GrpcClient {
    async fn embed(
        &self,
        texts: &[String],
        model: &str,
        route: &EmbedRoute,
    ) -> Result<Vec<Vec<f32>>, PvError> {
        let resp = self
            .with_failover(|mut client, budget| {
                let mut request = tonic::Request::new(EmbedTextsRequest {
                    texts: texts.to_vec(),
                    model: model.to_string(),
                    bridge_model: route.bridge_model.clone().unwrap_or_default(),
                    target_model: route.target_model.clone().unwrap_or_default(),
                    // Provider connectors consume `input_type`; hosts strip
                    // it before the engine (see the engine-path comment in
                    // embedded/server.rs). Set for both purposes: only
                    // `search_query` changes anything a gateway does, but
                    // the request should say what it means rather than
                    // treating "absent" as "document". Both postvec hosts
                    // strip the field on the engine path. A third-party
                    // inference node that honours it would apply templates.
                    input_type: route.purpose.as_wire().to_string(),
                    ..Default::default()
                });
                request.set_timeout(budget);
                async move { client.embed_texts(request).await.map(|r| r.into_inner()) }
            })
            .await?;
        let list = resp
            .embeddings
            .ok_or_else(|| PvError::Decode("missing embeddings in EmbedTextsResponse".into()))?;
        decode_embeddings(list)
    }

    async fn convert(&self, vecs: &[Vec<f32>], model: &str) -> Result<Vec<Vec<f32>>, PvError> {
        if vecs.iter().flatten().any(|f| !f.is_finite()) {
            return Err(PvError::InvalidInput(
                "convert input contains a non-finite component (NaN/Inf)".into(),
            ));
        }
        let embeddings: Vec<FloatVector> = vecs
            .iter()
            .map(|v| FloatVector { vector: v.clone() })
            .collect();
        let resp = self
            .with_failover(|mut client, budget| {
                // The wire keeps the bridge fields (the proto is a live
                // contract with services this extension does not ship), but
                // postvec plans direct conversions only, so they go empty.
                let mut request = tonic::Request::new(ConvertEmbeddingsRequest {
                    embeddings: embeddings.clone(),
                    model: model.to_string(),
                    source_model: String::new(),
                    bridge_model: String::new(),
                    target_model: String::new(),
                });
                request.set_timeout(budget);
                async move {
                    client
                        .convert_embeddings(request)
                        .await
                        .map(|r| r.into_inner())
                }
            })
            .await?;
        let list = resp.embeddings.ok_or_else(|| {
            PvError::Decode("missing embeddings in ConvertEmbeddingsResponse".into())
        })?;
        decode_embeddings(list)
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, PvError> {
        super::discovery::fetch_models(&self.http_endpoints, self.discovery_timeout).await
    }
}

impl GrpcClient {
    pub async fn list_models_report(&self) -> Result<super::discovery::DiscoveryReport, PvError> {
        super::discovery::fetch_models_report(&self.http_endpoints, self.discovery_timeout).await
    }

    /// How many sequential waves a full discovery pass may need under the
    /// bounded fan-out — the multiplier an overall refresh budget must apply
    /// to the per-node timeout so slow leading nodes cannot push a healthy
    /// later node past the caller's deadline.
    pub fn discovery_waves(&self) -> u64 {
        (self
            .http_endpoints
            .len()
            .div_ceil(super::discovery::DISCOVERY_CONCURRENCY))
        .max(1) as u64
    }
}

/// Where gRPC calls go for a given mode: the configured mesh endpoints, or —
/// embedded — only the in-worker loopback listener.
pub(crate) fn resolve_grpc_endpoints(
    mode: crate::gucs::Mode,
    embedded_listen: &str,
    configured: Vec<String>,
) -> Vec<String> {
    match mode {
        crate::gucs::Mode::Embedded => vec![embedded_listen.to_string()],
        crate::gucs::Mode::Grpc => configured,
    }
}

/// Where `GET /config` discovery goes for a given mode: the configured mesh
/// HTTP endpoints, or — embedded — only the engine host's loopback listener
/// (plain HTTP; the mesh's self-signed-HTTPS concern doesn't apply on
/// loopback).
pub(crate) fn resolve_http_endpoints(
    mode: crate::gucs::Mode,
    embedded_http_listen: &str,
    configured: Vec<String>,
) -> Vec<String> {
    match mode {
        crate::gucs::Mode::Embedded => vec![format!("http://{embedded_http_listen}")],
        crate::gucs::Mode::Grpc => configured,
    }
}

/// Map a tonic Status to PvError, honouring `x-ravenna-error-code` metadata.
pub fn status_to_error(endpoint: &str, status: &tonic::Status) -> PvError {
    if let Some(code) = status.metadata().get("x-ravenna-error-code") {
        if let Ok(code_str) = code.to_str() {
            let code = RavennaCode::parse(code_str);
            if code != RavennaCode::Unknown {
                return PvError::Remote {
                    code,
                    message: status.message().to_string(),
                };
            }
        }
    }
    match status.code() {
        tonic::Code::Unavailable | tonic::Code::Unknown => PvError::Transport {
            endpoint: endpoint.to_string(),
            message: status.message().to_string(),
        },
        tonic::Code::DeadlineExceeded => PvError::Remote {
            code: RavennaCode::Timeout,
            message: status.message().to_string(),
        },
        tonic::Code::InvalidArgument => PvError::Remote {
            code: RavennaCode::InvalidInput,
            message: status.message().to_string(),
        },
        tonic::Code::ResourceExhausted => {
            PvError::Decode(format!("gRPC resource exhausted: {}", status.message()))
        }
        // Fail-fast codes: these mean the endpoint is not (or is no longer) a
        // inference node — retrying max_retries times just delays the
        // inevitable and hides the misconfiguration. Internal => Permanent.
        tonic::Code::Unimplemented
        | tonic::Code::NotFound
        | tonic::Code::PermissionDenied
        | tonic::Code::Unauthenticated
        | tonic::Code::FailedPrecondition => PvError::Internal(format!(
            "gRPC {}: {} (is {endpoint} an inference node?)",
            status.code(),
            status.message()
        )),
        _ => PvError::Remote {
            code: RavennaCode::Unknown,
            message: format!("{}: {}", status.code(), status.message()),
        },
    }
}

/// Row/component ceilings for a decoded response, mirroring the request-side
/// item cap and the ≤ 48 MiB expected-response sub-batching. The 64 MiB
/// tonic decode cap already bounds the protobuf tree; these restate the
/// bound where the `Vec<Vec<f32>>` is actually built inside a PostgreSQL
/// backend, so a malformed or oversized response is a typed refusal before
/// the full copy exists.
const MAX_RESPONSE_ROWS: usize = 4096;
const MAX_RESPONSE_COMPONENTS: usize = 16 * 1024 * 1024;

/// `ListValue[ ListValue[ NumberValue ] ]` → `Vec<Vec<f32>>` (mirrors
/// the hosted gateway's list_value_to_json_value, but straight to floats).
pub fn decode_embeddings(list: ListValue) -> Result<Vec<Vec<f32>>, PvError> {
    use prost_types::value::Kind;
    if list.values.len() > MAX_RESPONSE_ROWS {
        return Err(PvError::Decode(format!(
            "response holds {} embedding rows; the client never requests more than \
             {MAX_RESPONSE_ROWS} per call",
            list.values.len()
        )));
    }
    let mut components: usize = 0;
    let mut out = Vec::with_capacity(list.values.len());
    for value in list.values {
        match value.kind {
            Some(Kind::ListValue(inner)) => {
                components = components.saturating_add(inner.values.len());
                if components > MAX_RESPONSE_COMPONENTS {
                    return Err(PvError::Decode(format!(
                        "response exceeds {MAX_RESPONSE_COMPONENTS} embedding components"
                    )));
                }
                let mut vec = Vec::with_capacity(inner.values.len());
                for v in inner.values {
                    match v.kind {
                        Some(Kind::NumberValue(n)) => vec.push(n as f32),
                        other => {
                            return Err(PvError::Decode(format!(
                                "inner embedding value was not a number: {other:?}"
                            )))
                        }
                    }
                }
                out.push(vec);
            }
            other => {
                return Err(PvError::Decode(format!(
                    "outer embedding value was not a list: {other:?}"
                )))
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::{value::Kind, Value};

    fn num(n: f64) -> Value {
        Value {
            kind: Some(Kind::NumberValue(n)),
        }
    }
    fn list(values: Vec<Value>) -> Value {
        Value {
            kind: Some(Kind::ListValue(ListValue { values })),
        }
    }

    #[test]
    fn decode_two_vectors() {
        let lv = ListValue {
            values: vec![
                list(vec![num(0.25), num(-1.5)]),
                list(vec![num(3.0), num(4.5)]),
            ],
        };
        let decoded = decode_embeddings(lv).unwrap();
        assert_eq!(decoded, vec![vec![0.25f32, -1.5], vec![3.0, 4.5]]);
    }

    #[test]
    fn decode_empty() {
        assert!(decode_embeddings(ListValue { values: vec![] })
            .unwrap()
            .is_empty());
    }

    #[test]
    fn decode_rejects_non_list_outer() {
        let lv = ListValue {
            values: vec![num(1.0)],
        };
        assert!(matches!(decode_embeddings(lv), Err(PvError::Decode(_))));
    }

    #[test]
    fn decode_rejects_non_number_inner() {
        let lv = ListValue {
            values: vec![list(vec![Value {
                kind: Some(Kind::StringValue("x".into())),
            }])],
        };
        assert!(matches!(decode_embeddings(lv), Err(PvError::Decode(_))));
    }

    #[test]
    fn status_with_ravenna_code_maps_to_remote() {
        let mut status = tonic::Status::internal("model gone");
        status
            .metadata_mut()
            .insert("x-ravenna-error-code", "MODEL_NOT_FOUND".parse().unwrap());
        let err = status_to_error("n1:33333", &status);
        match err {
            PvError::Remote { code, .. } => assert_eq!(code, RavennaCode::ModelNotFound),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn unknown_ravenna_metadata_falls_back_to_status_code() {
        let mut status = tonic::Status::invalid_argument("bad input");
        status
            .metadata_mut()
            .insert("x-ravenna-error-code", "NEW_CODE".parse().unwrap());
        let err = status_to_error("n1:33333", &status);
        match err {
            PvError::Remote { code, .. } => assert_eq!(code, RavennaCode::InvalidInput),
            other => panic!("expected Remote InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn resource_exhausted_is_permanent_decode_error() {
        let status = tonic::Status::resource_exhausted("message too large");
        assert!(matches!(
            status_to_error("n1:33333", &status),
            PvError::Decode(_)
        ));
    }

    #[test]
    fn unavailable_without_code_is_transport() {
        let status = tonic::Status::unavailable("connection refused");
        assert!(matches!(
            status_to_error("n1:33333", &status),
            PvError::Transport { .. }
        ));
    }

    #[test]
    fn model_config_errors_are_failover_eligible() {
        for code in [
            RavennaCode::ModelNotFound,
            RavennaCode::ModelNotLoaded,
            RavennaCode::ModelDisabled,
            RavennaCode::BridgePathNotFound,
            RavennaCode::ConverterNotFound,
            // Provider keys live per inference host: another node in the
            // fleet may hold a valid one.
            RavennaCode::UpstreamAuthFailed,
        ] {
            assert!(model_config_error_should_failover(&PvError::Remote {
                code,
                message: "try another node".into(),
            }));
        }
        assert!(!model_config_error_should_failover(&PvError::Remote {
            code: RavennaCode::InvalidInput,
            message: "bad request".into(),
        }));
    }

    #[test]
    fn convert_rejects_non_finite_input_before_endpoint_use() {
        let client = GrpcClient::new(Vec::new(), Vec::new(), 100, 100);
        let err = crate::runtime::block_on(client.convert(&[vec![f32::NAN]], "convert-model"))
            .unwrap_err();
        assert!(matches!(err, PvError::InvalidInput(_)));
    }

    /// Pointing the GUC at a gRPC service that does not speak the inference proto must surface
    /// immediately (Permanent), not burn max_retries on it.
    #[test]
    fn fail_fast_status_codes_are_permanent() {
        use crate::client::ErrorClass;
        for status in [
            tonic::Status::unimplemented("no such rpc"),
            tonic::Status::not_found("no such service"),
            tonic::Status::permission_denied("nope"),
            tonic::Status::unauthenticated("who?"),
            tonic::Status::failed_precondition("not ready"),
        ] {
            let err = status_to_error("n1:33333", &status);
            assert_eq!(
                err.class(),
                ErrorClass::Permanent,
                "{:?} must be permanent",
                status.code()
            );
        }
    }

    /// the engine's embed-bridge restriction refusal (deployment licence
    /// policy) arrives as FailedPrecondition + TARGET_RESTRICTED metadata:
    /// it must parse to the typed code and classify Permanent, so restricted
    /// jobs dead-letter with the real reason instead of retrying forever.
    #[test]
    fn target_restricted_metadata_is_permanent() {
        use crate::client::ErrorClass;
        let mut status = tonic::Status::failed_precondition(
            "embed-bridge target 'cohere-embed-v4.0' is not available for bridge embedding \
             on this deployment",
        );
        status.metadata_mut().insert(
            "x-ravenna-error-code",
            tonic::metadata::MetadataValue::from_static("TARGET_RESTRICTED"),
        );
        let err = status_to_error("n1:33333", &status);
        match &err {
            PvError::Remote { code, message } => {
                assert_eq!(*code, RavennaCode::TargetRestricted);
                assert!(message.contains("cohere-embed-v4.0"));
            }
            other => panic!("expected Remote(TargetRestricted), got {other:?}"),
        }
        assert_eq!(err.class(), ErrorClass::Permanent);
    }

    /// A malformed endpoint entry yields an error from channel construction
    /// (with_failover records it and moves on to the next node).
    #[test]
    fn invalid_endpoint_is_a_transport_error() {
        assert!(matches!(
            parse_endpoint("not a valid uri"),
            Err(PvError::Transport { .. })
        ));
    }

    /// SIGHUP topology churn: channels for endpoints no longer configured are
    /// evicted when a client is rebuilt from the GUCs.
    #[test]
    fn prune_channels_drops_removed_endpoints() {
        // connect_lazy still needs a runtime context to park its background
        // connection driver in — same as production, where channel_for only
        // ever runs inside runtime::block_on.
        crate::runtime::block_on(async {
            let _ = cache_lazy_channel("127.0.0.1:1");
            let _ = cache_lazy_channel("127.0.0.1:2");
        });
        prune_channels(&["127.0.0.1:2".to_string()]);
        let cache = CHANNELS.lock().unwrap();
        assert!(
            !cache.contains_key("127.0.0.1:1"),
            "removed endpoint evicted"
        );
        assert!(cache.contains_key("127.0.0.1:2"), "kept endpoint survives");
    }

    /// Embedded mode routes every backend gRPC call to the loopback listener
    /// regardless of (possibly stale) mesh endpoint GUCs.
    #[test]
    fn embedded_mode_overrides_grpc_endpoints() {
        use crate::gucs::Mode;
        let configured = vec!["192.0.2.2:33333".to_string(), "192.0.2.3:33333".to_string()];
        assert_eq!(
            resolve_grpc_endpoints(Mode::Grpc, "127.0.0.1:33433", configured.clone()),
            configured
        );
        assert_eq!(
            resolve_grpc_endpoints(Mode::Embedded, "127.0.0.1:33433", configured),
            vec!["127.0.0.1:33433".to_string()]
        );
        // No configured endpoints at all: embedded still has somewhere to go.
        assert_eq!(
            resolve_grpc_endpoints(Mode::Embedded, "127.0.0.1:33433", Vec::new()),
            vec!["127.0.0.1:33433".to_string()]
        );
    }

    /// Embedded mode routes HTTP discovery to the engine host's loopback
    /// /config listener (plain http://), regardless of mesh HTTP GUCs.
    #[test]
    fn embedded_mode_overrides_http_endpoints() {
        use crate::gucs::Mode;
        let configured = vec!["https://192.0.2.2:22222".to_string()];
        assert_eq!(
            resolve_http_endpoints(Mode::Grpc, "127.0.0.1:33434", configured.clone()),
            configured
        );
        assert_eq!(
            resolve_http_endpoints(Mode::Embedded, "127.0.0.1:33434", configured),
            vec!["http://127.0.0.1:33434".to_string()]
        );
        assert_eq!(
            resolve_http_endpoints(Mode::Embedded, "127.0.0.1:33434", Vec::new()),
            vec!["http://127.0.0.1:33434".to_string()]
        );
    }

    /// The failover loop shares one total deadline: the caller-side budget is
    /// the configured timeout (+1 s setup slack) regardless of endpoint count
    /// — an operator's "30 second timeout" means 30 seconds, not 30 × nodes.
    #[test]
    fn overall_timeout_is_endpoint_count_independent() {
        let one = GrpcClient::new(vec!["n1:33333".into()], Vec::new(), 2_000, 500);
        assert_eq!(one.overall_timeout_ms(), 3_000);

        let three = GrpcClient::new(
            vec!["n1:33333".into(), "n2:33333".into(), "n3:33333".into()],
            Vec::new(),
            2_000,
            500,
        );
        assert_eq!(three.overall_timeout_ms(), 3_000);
    }

    /// Endpoint lists deduplicate (first occurrence wins) and cap at
    /// MAX_LIST_ENTRIES so a repeated or unbounded list cannot multiply the
    /// failover/discovery fan-out.
    #[test]
    fn endpoints_dedup_and_cap() {
        let dup = GrpcClient::new(
            vec!["n1:33333".into(), "n2:33333".into(), "n1:33333".into()],
            Vec::new(),
            2_000,
            500,
        );
        assert_eq!(dup.grpc_endpoints, vec!["n1:33333", "n2:33333"]);

        let many: Vec<String> = (0..100).map(|i| format!("n{i}:33333")).collect();
        let capped = GrpcClient::new(many, Vec::new(), 2_000, 500);
        assert_eq!(capped.grpc_endpoints.len(), crate::gucs::MAX_LIST_ENTRIES);
    }

    /// End-to-end failover through the real request loop, with live tonic
    /// servers. Server stubs are only generated under the `embedded` feature,
    /// so these run in the `--features embedded` suite (part of ./ci.sh).
    #[cfg(feature = "embedded")]
    mod failover_loop {
        use super::*;
        use crate::client::ErrorClass;
        use crate::proto::ninference_service_server::{NinferenceService, NinferenceServiceServer};
        use crate::proto::{
            ConvertEmbeddingsRequest, ConvertEmbeddingsResponse, EmbedTextsResponse,
        };
        use std::sync::Arc;
        use tonic::{Request, Response, Status};

        /// An inference node whose EmbedTexts answers depend on a *shared*
        /// call counter: calls whose global index is below `fail_first` fail
        /// with the structured bridge-inventory code; later calls answer one
        /// embedding. Sharing the counter between nodes makes the failover
        /// test order-independent — whichever node the round-robin picks
        /// first sees call 0 and fails — so the test never touches the
        /// process-global `ROUND_ROBIN` (racy under parallel test threads).
        struct StubNode {
            fail_code: &'static str,
            fail_first: usize,
            calls: Arc<AtomicUsize>,
            /// Delay before answering — a slow-but-valid node.
            delay: Duration,
            /// Records the inbound `grpc-timeout` header of the last call.
            observed_timeout: Arc<std::sync::Mutex<Option<String>>>,
        }

        #[tonic::async_trait]
        impl NinferenceService for StubNode {
            async fn embed_texts(
                &self,
                _request: Request<EmbedTextsRequest>,
            ) -> Result<Response<EmbedTextsResponse>, Status> {
                *self.observed_timeout.lock().unwrap() = _request
                    .metadata()
                    .get("grpc-timeout")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.to_string());
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n < self.fail_first {
                    let mut status =
                        Status::failed_precondition("stub: bridge chain incomplete on this node");
                    status.metadata_mut().insert(
                        "x-ravenna-error-code",
                        tonic::metadata::MetadataValue::from_static(self.fail_code),
                    );
                    return Err(status);
                }
                Ok(Response::new(EmbedTextsResponse {
                    embeddings: Some(ListValue {
                        values: vec![list(vec![num(1.0), num(2.0), num(3.0)])],
                    }),
                    usage: None,
                }))
            }

            async fn convert_embeddings(
                &self,
                _request: Request<ConvertEmbeddingsRequest>,
            ) -> Result<Response<ConvertEmbeddingsResponse>, Status> {
                Err(Status::unimplemented("stub"))
            }
        }

        /// Serve a stub on an ephemeral loopback port; returns its address.
        /// Must be called from within a tokio runtime.
        fn spawn_stub(
            fail_code: &'static str,
            fail_first: usize,
            calls: Arc<AtomicUsize>,
        ) -> String {
            spawn_stub_with_delay(fail_code, fail_first, calls, Duration::ZERO)
        }

        fn spawn_stub_with_delay(
            fail_code: &'static str,
            fail_first: usize,
            calls: Arc<AtomicUsize>,
            delay: Duration,
        ) -> String {
            spawn_stub_recording(fail_code, fail_first, calls, delay, Default::default())
        }

        fn spawn_stub_recording(
            fail_code: &'static str,
            fail_first: usize,
            calls: Arc<AtomicUsize>,
            delay: Duration,
            observed_timeout: Arc<std::sync::Mutex<Option<String>>>,
        ) -> String {
            let svc = NinferenceServiceServer::new(StubNode {
                fail_code,
                fail_first,
                calls,
                delay,
                observed_timeout,
            });
            let incoming =
                tonic::transport::server::TcpIncoming::bind("127.0.0.1:0".parse().unwrap())
                    .expect("bind ephemeral loopback port");
            let addr = incoming.local_addr().expect("local_addr");
            tokio::spawn(async move {
                let _ = tonic::transport::Server::builder()
                    .add_service(svc)
                    .serve_with_incoming(incoming)
                    .await;
            });
            format!("127.0.0.1:{}", addr.port())
        }

        fn test_runtime() -> tokio::runtime::Runtime {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
        }

        /// Two nodes share the call counter and fail only the first call
        /// globally with BRIDGE_PATH_NOT_FOUND: whichever node the
        /// round-robin picks first fails, the loop moves to the other node,
        /// and its answer comes back. Order-independent by construction.
        #[test]
        fn bridge_inventory_error_fails_over_to_next_node() {
            let rt = test_runtime();
            rt.block_on(async {
                let calls = Arc::new(AtomicUsize::new(0));
                let addr_a = spawn_stub("BRIDGE_PATH_NOT_FOUND", 1, calls.clone());
                let addr_b = spawn_stub("BRIDGE_PATH_NOT_FOUND", 1, calls.clone());
                let client = GrpcClient::new(vec![addr_a, addr_b], Vec::new(), 5_000, 100);

                let out = client
                    .embed(
                        &["hello".to_string()],
                        "embed-bridge",
                        &EmbedRoute {
                            bridge_model: Some("m".into()),
                            target_model: Some("ext".into()),
                            ..Default::default()
                        },
                    )
                    .await
                    .expect("failover must recover on the second node");

                assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    2,
                    "first-selected node failed with the structured code; \
                     failover reached the second"
                );
            });
        }

        /// Every node reports the structured code: the typed error must
        /// surface (not a generic transport/internal error) and classify
        /// Config — retryable inventory state, not a permanent dead-letter.
        #[test]
        fn bridge_inventory_error_on_all_nodes_surfaces_typed_config_error() {
            let rt = test_runtime();
            rt.block_on(async {
                let calls = Arc::new(AtomicUsize::new(0));
                let addr = spawn_stub("CONVERTER_NOT_FOUND", usize::MAX, calls.clone());
                let client = GrpcClient::new(vec![addr], Vec::new(), 5_000, 100);

                let err = client
                    .embed(
                        &["hello".to_string()],
                        "convert-bridge",
                        &EmbedRoute::default(),
                    )
                    .await
                    .expect_err("single failing node must surface the error");

                assert_eq!(calls.load(Ordering::SeqCst), 1);
                match &err {
                    PvError::Remote { code, .. } => {
                        assert_eq!(*code, RavennaCode::ConverterNotFound)
                    }
                    other => panic!("expected Remote(ConverterNotFound), got {other:?}"),
                }
                assert_eq!(err.class(), ErrorClass::Config);
            });
        }

        /// The client must send its attempt budget as `grpc-timeout`. tonic
        /// only emits that when the call wraps a `tonic::Request` with
        /// `set_timeout`. Without it the loopback server's inbound-deadline
        /// clamp never engages, so a 2 s `search()` would grant the engine
        /// the 30 s default.
        #[test]
        fn client_propagates_its_deadline_as_grpc_timeout() {
            let rt = test_runtime();
            rt.block_on(async {
                let observed: Arc<std::sync::Mutex<Option<String>>> = Default::default();
                let addr = spawn_stub_recording(
                    "UNUSED",
                    0,
                    Arc::new(AtomicUsize::new(0)),
                    Duration::ZERO,
                    observed.clone(),
                );
                let client = GrpcClient::new(vec![addr], Vec::new(), 2_000, 100);
                client
                    .embed(&["hello".to_string()], "m", &EmbedRoute::default())
                    .await
                    .expect("stub answers");
                let seen = observed.lock().unwrap().clone();
                let seen = seen.expect("the server received a grpc-timeout header");
                // Format: digits + unit. The budget is the ~2 s attempt.
                let (digits, unit) = seen.split_at(seen.len() - 1);
                let v: u64 = digits.parse().expect("numeric grpc-timeout");
                let ms = match unit {
                    "m" => v,
                    "S" => v * 1_000,
                    "u" => v / 1_000,
                    "n" => v / 1_000_000,
                    other => panic!("unexpected grpc-timeout unit {other:?}"),
                };
                assert!(
                    ms > 0 && ms <= 2_000,
                    "propagated deadline within the configured budget: {seen}"
                );
            });
        }

        /// A slow-but-valid first node must receive the full inference
        /// deadline, not a slice of remaining / attempts_left. Splitting a
        /// 2 s budget across two nodes times out a healthy 1.2 s answer
        /// and repeats the inference elsewhere.
        #[test]
        fn slow_but_valid_first_node_keeps_the_full_inference_deadline() {
            let rt = test_runtime();
            rt.block_on(async {
                // BOTH nodes are slow-but-valid and share one call counter,
                // so the test is independent of the global round-robin's
                // starting index: whichever node is selected first must be
                // allowed its full 1.2 s answer inside the 2 s budget.
                let calls = Arc::new(AtomicUsize::new(0));
                let a =
                    spawn_stub_with_delay("UNUSED", 0, calls.clone(), Duration::from_millis(1_200));
                let b =
                    spawn_stub_with_delay("UNUSED", 0, calls.clone(), Duration::from_millis(1_200));
                let client = GrpcClient::new(vec![a, b], Vec::new(), 2_000, 100);
                let out = client
                    .embed(&["hello".to_string()], "m", &EmbedRoute::default())
                    .await
                    .expect("the slow-but-valid first node answers within the FULL deadline");
                assert_eq!(out, vec![vec![1.0, 2.0, 3.0]]);
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "no premature failover repeated the inference elsewhere"
                );
            });
        }
    }
}
