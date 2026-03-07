//! The provider gateway (feature `wire`): the single implementation both
//! inference hosts mount — the embedded engine host inside postvec.so and
//! postvec-server.
//!
//! It owns the loaded `providers.d` state behind an `Arc` swap: `reload`
//! builds a complete new snapshot and swaps it in atomically; in-flight
//! `embed` calls hold the previous `Arc` and finish against it, and a failed
//! reload keeps the previous snapshot (never half-applies). Every provider
//! failure leaves here as a [`GatewayError`] carrying a `shared::ErrorCode`
//! — the wire error-code vocabulary the extension already classifies — per
//! the normative mapping table in docs/external-providers.md §6.4.

use crate::config::{self, LoadOutcome, ModelDescriptor};
use crate::{new_embedding_backend, EmbeddingBackend, EmbeddingError};
use shared::ErrorCode;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tokio::sync::Semaphore;

/// Query-vs-document purpose, carried on the wire as
/// `EmbedTextsRequest.input_type` ("search_query" / "search_document").
/// Only Cohere consumes it today; everything else embeds identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputType {
    #[default]
    Document,
    Query,
}

impl InputType {
    /// Wire mapping: only the exact `search_query` marker selects the query
    /// backend; absent/unknown values keep today's document behavior.
    pub fn from_wire(raw: &str) -> Self {
        if raw == "search_query" {
            InputType::Query
        } else {
            InputType::Document
        }
    }
}

/// A provider failure in the extension's wire vocabulary. The message is
/// secret-free by construction (client errors are scrubbed at creation).
#[derive(Debug)]
pub struct GatewayError {
    pub code: ErrorCode,
    pub message: String,
}

impl GatewayError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for GatewayError {}

/// What a (re)load accomplished. `errors` are per-file/per-provider failures
/// that were isolated (the rest of the directory still serves).
#[derive(Debug, Default)]
pub struct ReloadReport {
    pub providers: usize,
    pub models: usize,
    pub errors: Vec<String>,
}

struct ModelEntry {
    /// Operator-facing provider name (the file stem) — log vocabulary.
    provider_name: String,
    /// Per-provider outbound concurrency cap, shared by every model in the
    /// same provider file. This is the ONLY admission control on the
    /// provider path (the hosts bypass their engine gates for it, §7.3).
    permits: Arc<Semaphore>,
    /// The configured cap behind `permits` — `available_permits()` shrinks
    /// while embeds are in flight, so budget math must never read it.
    max_concurrent: usize,
    document: Box<dyn EmbeddingBackend>,
    /// Present only where purpose changes the request (Cohere); everything
    /// else serves queries through the document backend.
    query: Option<Box<dyn EmbeddingBackend>>,
    dim: u32,
    max_batch: usize,
    /// The §6.2 nested HubModel descriptor served in `/config`.
    descriptor: serde_json::Value,
}

impl ModelEntry {
    fn backend(&self, input_type: InputType) -> &dyn EmbeddingBackend {
        match input_type {
            InputType::Query => self.query.as_deref().unwrap_or(self.document.as_ref()),
            InputType::Document => self.document.as_ref(),
        }
    }
}

#[derive(Default)]
struct Inner {
    models: BTreeMap<String, ModelEntry>,
}

/// A stable, secret-free digest of the endpoint a provider file will actually
/// reach: the AWS region and the `base_url` override, which together are the
/// only things that decide *where* a request goes once the connector type and
/// model id are fixed.
///
/// Hashed rather than published because `base_url` is operator-supplied and
/// routinely names an internal host, while `/config` is read by every node in
/// a fleet. A digest answers the only question a fleet needs to ask — "do all
/// the nodes reach the same place?" — without publishing the answer.
fn endpoint_digest(region: Option<&str>, base_url: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(region.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(base_url.unwrap_or("").as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

/// The nested HubModel shape `discovery::parse_config` requires (§6.2). A
/// flat object would be silently dropped by the parser — the round-trip
/// tests below and in the host pin this.
fn descriptor_json(
    provider_type: &str,
    provider_name: &str,
    endpoint: &str,
    model: &ModelDescriptor,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "model_type": "embed",
        "target_model": model.name,
        "target_dim": model.dim,
    });
    if let Some(max_tokens) = model.max_tokens {
        params["sequence_len"] = serde_json::json!(max_tokens);
    }
    serde_json::json!({
        "name": model.name,
        "status": "provider",
        "provider": provider_type,
        // The routing identity behind the public name, secret-free: the file
        // stem an operator administers and the id the provider's API is
        // asked for. A provider-backed model has no on-disk descriptor to
        // compare, so without these two a fleet check can only compare
        // *names* — and two nodes serving `openai-text-embedding-3-small`
        // from different files, model ids or dimensions look identical while
        // round-robin hands a caller vectors from either.
        "provider_file": provider_name,
        "provider_model_id": model.provider_model_id,
        // Region and base_url, digested — the rest of "where does this go".
        "provider_endpoint": endpoint,
        "configuration": {
            "name": model.name,
            "enabled": true,
            "params": params,
        }
    })
}

impl Inner {
    /// Build a snapshot from a scanned directory. Provider-level factory
    /// failures are isolated to that provider, mirroring the per-file rule.
    fn build(
        outcome: LoadOutcome,
        local_models: &std::collections::BTreeSet<String>,
    ) -> (Inner, Vec<String>) {
        let mut errors: Vec<String> = outcome.errors.iter().map(|e| e.to_string()).collect();
        let mut inner = Inner::default();

        'providers: for provider in outcome.providers {
            let provider_type = crate::catalog::canonical_provider(&provider.config.provider);
            // Plaintext to something other than loopback puts the provider
            // credential on the wire in the clear. Not refused — a
            // self-hosted OpenAI-compatible endpoint on a private network is
            // a real deployment, and loopback is the ordinary sidecar/mock
            // shape — but never silent either. `postvec doctor` says the
            // same thing where an operator will actually see it.
            if provider
                .config
                .base_url
                .as_deref()
                .is_some_and(config::base_url_is_plaintext_offhost)
            {
                log::warn!(
                    "providers.d: provider {:?} has a plaintext (http://) base_url on a \
                     non-loopback host; its credential crosses the network unencrypted",
                    provider.name
                );
            }
            // One fresh reqwest client per provider, rustls via this crate's
            // feature set. Deliberately NOT any shared/discovery client: the
            // hosts' discovery client disables certificate validation, and
            // provider calls carry credentials over the public internet.
            let client = match reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(provider.timeout_ms))
                // No redirects. An embeddings POST has no legitimate reason
                // to be redirected, and following one carries the credential
                // to wherever the response points. `reqwest` strips
                // `Authorization` across origins but not a provider-specific
                // auth header — Gemini's `x-goog-api-key` would ride along —
                // so this is the rule that holds for every connector rather
                // than for most of them. A moved endpoint is a `base_url`
                // change, which is an operator decision with a privacy gate
                // in front of it, not something an upstream announces.
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(client) => client,
                Err(e) => {
                    errors.push(format!("provider {:?}: http client: {e}", provider.name));
                    continue;
                }
            };
            let permits = Arc::new(Semaphore::new(provider.max_concurrent));
            let endpoint = endpoint_digest(
                provider.config.region.as_deref(),
                provider.config.base_url.as_deref(),
            );

            // A provider file claiming a name the engine already has is
            // refused, not quietly shadowed. Shadowing was deterministic —
            // local wins — but it left a *dormant* provider entry behind the
            // local one, and unloading or deactivating the local model made
            // that entry start serving: a column's source text began leaving
            // the host with no `enable()` NOTICE and no `provider add`
            // acknowledgement, because neither ran. Refusing keeps the same
            // "local by default" outcome and removes the dormant state
            // instead of scheduling it.
            if let Some(clash) = provider
                .models
                .iter()
                .find(|m| local_models.contains(&m.name))
            {
                errors.push(format!(
                    "provider {:?}: model {:?} is already served by a local engine model; a \
                     public name must have exactly one owner. Rename the [[models]] entry — \
                     leaving it would let the provider take the name over the moment the \
                     local model is unloaded",
                    provider.name, clash.name
                ));
                continue 'providers;
            }

            let mut entries = Vec::with_capacity(provider.models.len());
            for model in &provider.models {
                let build = |input_type: &str| {
                    new_embedding_backend(
                        &provider.config,
                        &model.provider_model_id,
                        model.dim as i32,
                        input_type,
                        Some(client.clone()),
                    )
                };
                let document = match build("search_document") {
                    Ok(backend) => backend,
                    Err(e) => {
                        // A factory failure is configuration-shaped (bad
                        // provider type, missing credential field): it
                        // affects every model in the file identically, so
                        // skip the provider as a unit.
                        errors.push(format!("provider {:?}: {e}", provider.name));
                        continue 'providers;
                    }
                };
                // Purpose changes the request body for Cohere (`input_type`)
                // and for Gemini (`taskType`); one backend per purpose keeps
                // it a constructor concern (the copied client's shape)
                // instead of a per-call trait parameter.
                //
                // Google belongs in this list. Adding `taskType` to the
                // client without adding it here meant every Gemini query was
                // still embedded as `RETRIEVAL_DOCUMENT` — the parameter was
                // built and never reached, which is worse than not having it,
                // because the descriptor claims the distinction is honoured.
                let query = if matches!(provider_type.as_str(), "cohere" | "google") {
                    match build("search_query") {
                        Ok(backend) => Some(backend),
                        Err(e) => {
                            errors.push(format!("provider {:?}: {e}", provider.name));
                            continue 'providers;
                        }
                    }
                } else {
                    None
                };
                entries.push((
                    model.name.clone(),
                    ModelEntry {
                        provider_name: provider.name.clone(),
                        permits: permits.clone(),
                        max_concurrent: provider.max_concurrent,
                        document,
                        query,
                        dim: model.dim,
                        max_batch: model.max_batch,
                        descriptor: descriptor_json(
                            &provider_type,
                            &provider.name,
                            &endpoint,
                            model,
                        ),
                    },
                ));
            }
            for (name, entry) in entries {
                inner.models.insert(name, entry);
            }
        }
        (inner, errors)
    }
}

/// The provider gateway. Cheap to share (`Arc<Gateway>`); all mutation is
/// the atomic snapshot swap in [`Gateway::reload`].
pub struct Gateway {
    inner: RwLock<Arc<Inner>>,
}

impl Gateway {
    /// An empty gateway: owns nothing, serves nothing. The zero-config state.
    pub fn empty() -> Self {
        Gateway {
            inner: RwLock::new(Arc::new(Inner::default())),
        }
    }

    /// Load a providers.d directory. Failures — structural or per-file —
    /// are logged and isolated; the returned gateway always exists and
    /// serves whatever loaded (possibly nothing). A missing directory is
    /// the ordinary zero-config case and logs nothing.
    pub fn load(dir: &Path, local_models: &std::collections::BTreeSet<String>) -> Self {
        let gateway = Gateway::empty();
        match gateway.reload(dir, local_models) {
            Ok(report) => {
                for error in &report.errors {
                    log::warn!("providers.d: {error}");
                }
                if report.models > 0 {
                    log::info!(
                        "provider gateway: serving {} model(s) from {} provider(s) under {}",
                        report.models,
                        report.providers,
                        dir.display()
                    );
                }
            }
            Err(e) => log::warn!("provider gateway: {e}; serving no provider models"),
        }
        gateway
    }

    /// Rescan the directory and swap the snapshot in atomically. In-flight
    /// `embed` calls hold the previous `Arc` and finish against it.
    ///
    /// `Err` = structural failure (the directory exists but cannot be
    /// scanned): the previous snapshot is KEPT, never half-replaced.
    /// Per-file failures are isolated as before and reported in the `Ok`.
    pub fn reload(
        &self,
        dir: &Path,
        local_models: &std::collections::BTreeSet<String>,
    ) -> Result<ReloadReport, String> {
        let outcome = config::load_dir(dir)?;
        let (inner, errors) = Inner::build(outcome, local_models);
        let report = ReloadReport {
            providers: inner
                .models
                .values()
                .map(|entry| entry.provider_name.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            models: inner.models.len(),
            errors,
        };
        *self.inner.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(inner);
        Ok(report)
    }

    fn snapshot(&self) -> Arc<Inner> {
        self.inner.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Does this gateway serve `model`? The hosts consult this BEFORE
    /// `is_model_ready` — a provider name is never in the engine.
    pub fn owns(&self, model: &str) -> bool {
        self.snapshot().models.contains_key(model)
    }

    /// True when nothing is configured — hosts use this to keep the
    /// zero-config path byte-identical to today's behavior.
    pub fn is_empty(&self) -> bool {
        self.snapshot().models.is_empty()
    }

    /// Declared dimension of a served model (hosts size response envelopes
    /// from it before dispatch).
    pub fn dim(&self, model: &str) -> Option<u32> {
        self.snapshot().models.get(model).map(|entry| entry.dim)
    }

    /// Sum of the per-provider `max_concurrent` caps — the §7.3
    /// `provider_inflight_budget` the hosts add to their ingress limit.
    /// Reads the configured caps, never live semaphore state, so the number
    /// is stable regardless of in-flight embeds.
    pub fn inflight_budget(&self) -> usize {
        let snapshot = self.snapshot();
        let mut per_provider: BTreeMap<&str, usize> = BTreeMap::new();
        for entry in snapshot.models.values() {
            per_provider
                .entry(entry.provider_name.as_str())
                .or_insert(entry.max_concurrent);
        }
        per_provider.values().sum()
    }

    /// The §6.2 descriptors for the `/config` merge, in name order.
    pub fn models(&self) -> Vec<serde_json::Value> {
        self.snapshot()
            .models
            .values()
            .map(|entry| entry.descriptor.clone())
            .collect()
    }

    /// Embed `texts` with a provider-backed model. Sub-batches by the
    /// descriptor's `max_batch`, enforces the per-provider `max_concurrent`
    /// semaphore, and validates count and dimension on every response.
    pub async fn embed(
        &self,
        model: &str,
        texts: &[String],
        input_type: InputType,
        deadline: Instant,
    ) -> Result<Vec<Vec<f32>>, GatewayError> {
        // Snapshot once: a concurrent reload cannot change the world under us.
        let snapshot = self.snapshot();
        let entry = snapshot.models.get(model).ok_or_else(|| {
            GatewayError::new(
                ErrorCode::ModelNotFound,
                format!("model {model:?} is not served by any configured provider"),
            )
        })?;

        // An empty input is a *row* problem, and providers report it as a
        // batch one. OpenAI documents `input` as required to be non-empty and
        // answers a whole request containing `""` with a 400 — so
        // `[good, "", good]` would map to InvalidInput and dead-letter three
        // rows for one bad one. Classifying it here as ContextLengthExceeded
        // hands it to the queue's bisection, which halves the batch until the
        // empty row is alone and dead-letters only that. Checked before the
        // request rather than after: no reason to pay for the round trip.
        if let Some(position) = texts.iter().position(|text| text.is_empty()) {
            return Err(GatewayError::new(
                ErrorCode::ContextLengthExceeded,
                format!(
                    "input {position} is empty, which providers reject for the whole request; \
                     isolating it rather than failing its neighbours"
                ),
            ));
        }

        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(entry.max_batch.max(1)) {
            if Instant::now() >= deadline {
                return Err(GatewayError::new(
                    ErrorCode::Timeout,
                    "deadline exhausted before the provider request",
                ));
            }
            // The per-provider semaphore is the provider path's ONLY
            // admission gate; waiting for it is bounded by the caller's
            // deadline like everything else.
            let _permit = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                entry.permits.clone().acquire_owned(),
            )
            .await
            .map_err(|_| {
                GatewayError::new(
                    ErrorCode::Timeout,
                    format!(
                        "deadline exhausted waiting for provider {:?} capacity",
                        entry.provider_name
                    ),
                )
            })?
            .map_err(|_| {
                GatewayError::new(ErrorCode::InternalError, "provider semaphore closed")
            })?;

            let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
            let embeddings = entry
                .backend(input_type)
                .embed(&refs, Some(deadline))
                .await
                .map_err(|e| map_embedding_error(&entry.provider_name, &e))?;

            // Response-shape contract (§6.4): systematic count or dimension
            // mismatch is InvalidInput (Permanent), never InternalError —
            // InternalError classifies Transient and would burn the queue's
            // whole retry budget before dead-lettering every row anyway.
            if embeddings.len() != chunk.len() {
                return Err(GatewayError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "provider {:?} returned {} embeddings for {} inputs",
                        entry.provider_name,
                        embeddings.len(),
                        chunk.len()
                    ),
                ));
            }
            for (position, embedding) in embeddings.into_iter().enumerate() {
                // Alignment, not just arity. Every client reports which input
                // a vector belongs to (`text_index`: the provider's own
                // `index` field for the OpenAI-shaped APIs, the response
                // position for the rest) and then hands them back in input
                // order. A response that repeats or skips an index survives
                // the count and dimension checks above and would write one
                // row's vector onto another row — silently, and permanently,
                // since nothing downstream can detect it. Checking the field
                // the clients already carry is what makes "row-parallel to
                // the input" an assertion rather than an assumption.
                if embedding.text_index != position {
                    return Err(GatewayError::new(
                        ErrorCode::InvalidInput,
                        format!(
                            "provider {:?} returned a vector for input {} at position \
                             {position} for {model:?}; the response is not row-parallel to \
                             the request",
                            entry.provider_name, embedding.text_index
                        ),
                    ));
                }
                if embedding.vector.len() as u32 != entry.dim {
                    return Err(GatewayError::new(
                        ErrorCode::InvalidInput,
                        format!(
                            "provider {:?} returned a {}-dim vector for {model:?} \
                             (declared dim {})",
                            entry.provider_name,
                            embedding.vector.len(),
                            entry.dim
                        ),
                    ));
                }
                vectors.push(embedding.vector);
            }
        }
        Ok(vectors)
    }

    /// Test hook: the per-provider semaphore behind a served model.
    #[cfg(any(test, feature = "test-util"))]
    pub fn permits_for_test(&self, model: &str) -> Option<Arc<Semaphore>> {
        self.snapshot()
            .models
            .get(model)
            .map(|entry| entry.permits.clone())
    }
}

/// The §6.4 mapping table, normative. `provider` is the operator-facing
/// file-stem name — safe log vocabulary, never a secret.
fn map_embedding_error(provider: &str, e: &EmbeddingError) -> GatewayError {
    let code = match e {
        // Every network-layer failure is transient, with no inspection of
        // reqwest's error kind. `is_decode()` looks like it would separate
        // "the body was garbage" from "the connection died", and it does
        // not: reqwest gives a body-stream failure the same Decode kind as
        // a serde failure, so keying on it would dead-letter a batch whose
        // only problem was a reset connection. The clients keep the two
        // apart at the source instead — `bytes()` failures stay here, and a
        // body that will not parse arrives as `Api { status: 200 }` below
        // (see `crate::decode_json`).
        EmbeddingError::Network(_) => ErrorCode::UpstreamServiceUnavailable,
        // 408 and 424 are here because of Bedrock, which is the only
        // supported provider that uses them: `ModelTimeoutException` is HTTP
        // 408 and `ModelErrorException` is HTTP 424 on `InvokeModel`. Both
        // describe the *model* failing to answer, not the row failing to be
        // acceptable, so classifying them with the rest of 4xx would
        // dead-letter healthy rows over an upstream hiccup.
        EmbeddingError::Api {
            status: 408 | 424 | 429 | 500..=599,
            ..
        } => ErrorCode::UpstreamServiceUnavailable,
        // Bad or revoked credential: an ops problem, classified Config
        // extension-side (bounded retry + failover, no dead-lettering).
        EmbeddingError::Api {
            status: 401 | 403, ..
        } => ErrorCode::UpstreamAuthFailed,
        // Unknown model id at the provider: Config (retried, failover).
        EmbeddingError::Api { status: 404, .. } => ErrorCode::ModelNotFound,
        // 413 is definitionally "your request was too big", with no wording to
        // interpret — so it maps to PoisonRow unconditionally and lets the
        // queue's bisection shrink the batch and isolate the offending row.
        EmbeddingError::Api { status: 413, .. } => ErrorCode::ContextLengthExceeded,
        // A 400/422 the connector already classified from the body it read.
        // The decision is made where the body is, so the body never has to
        // travel here — which is what lets the public error carry no upstream
        // text at all (see `crate::api_error`).
        EmbeddingError::InputTooLong { .. } => ErrorCode::ContextLengthExceeded,
        // Every remaining **request-side** 4xx is attributed to the rows
        // until bisection proves otherwise.
        //
        // The line this draws is between a rejected *request* and a malformed
        // *response*. A 4xx says the provider looked at what we sent and
        // refused it, and providers refuse a whole request over one bad item
        // — OpenAI documents `input` as required to be non-empty and answers
        // `[good, "", good]` with a single 400. Dead-lettering the batch there
        // destroys healthy rows for one poisoned neighbour, and the operator
        // has no way to tell which. PoisonRow costs a bounded log2(n) extra
        // calls when the fault really is batch-wide (4xx are not billed for
        // tokens) and ends in the same place; when it is one row, it isolates
        // that row and saves the rest. The asymmetry decides it.
        EmbeddingError::Api { status, .. } if (400..500).contains(status) => {
            ErrorCode::ContextLengthExceeded
        }
        // Everything else, including the clients' 200-decode-failure
        // sentinel: a response the provider produced, which is not about any
        // row and cannot be bisected into working.
        EmbeddingError::Api { .. } => ErrorCode::InvalidInput,
        EmbeddingError::Deserialization(_) => ErrorCode::InvalidInput,
        EmbeddingError::Deadline(_) => ErrorCode::Timeout,
        // Unreachable after a successful load (credentials resolve at load
        // time); classified as auth/internal so nothing dead-letters data
        // over a host-side misconfiguration.
        EmbeddingError::Authentication(_) => ErrorCode::UpstreamAuthFailed,
        EmbeddingError::Configuration(_) => ErrorCode::InternalError,
    };
    GatewayError::new(code, format!("provider {provider:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as mock;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// A providers.d is 0700; `tempfile::tempdir()` honours the umask.
    /// No local engine model in scope: the ordinary case for these tests.
    fn no_local() -> std::collections::BTreeSet<String> {
        Default::default()
    }

    fn private_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn write_provider(dir: &Path, file: &str, body: &str) {
        let path = dir.join(file);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn openai_toml(base_url: &str, dim: u32, max_batch: usize) -> String {
        format!(
            "provider = \"openai\"\napi_key = \"sk-test\"\nbase_url = \"{base_url}\"\n\
             max_concurrent = 1\n\n[[models]]\nname = \"openai-text-embedding-3-small\"\n\
             provider_model_id = \"text-embedding-3-small\"\ndim = {dim}\nmax_batch = {max_batch}\n\
             max_tokens = 8191\n"
        )
    }

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    /// The §6.2 shape, asserted field by field — the crate-side twin of the
    /// host's round-trip through `discovery::parse_config` (a flat object
    /// would be silently dropped there).
    #[tokio::test]
    async fn descriptors_use_the_nested_hubmodel_shape() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "openai.toml",
            &openai_toml("http://127.0.0.1:1", 1536, 512),
        );
        let gateway = Gateway::load(dir.path(), &no_local());

        let models = gateway.models();
        assert_eq!(models.len(), 1);
        let m = &models[0];
        assert_eq!(m["name"], "openai-text-embedding-3-small");
        assert_eq!(m["status"], "provider");
        assert_eq!(m["provider"], "openai");
        assert_eq!(m["configuration"]["name"], "openai-text-embedding-3-small");
        assert_eq!(m["configuration"]["enabled"], true);
        let params = &m["configuration"]["params"];
        assert_eq!(params["model_type"], "embed");
        assert_eq!(params["target_model"], "openai-text-embedding-3-small");
        assert_eq!(params["target_dim"], 1536);
        assert_eq!(params["sequence_len"], 8191);
        // Nothing embedding-relevant at the top level: the parser reads
        // configuration.* only.
        assert!(m.get("enabled").is_none());
        assert!(m.get("target_dim").is_none());

        assert!(gateway.owns("openai-text-embedding-3-small"));
        assert!(!gateway.owns("no-such-model"));
        assert!(!gateway.is_empty());
        assert_eq!(gateway.inflight_budget(), 1);
    }

    #[tokio::test]
    async fn gemini_alias_normalizes_to_google_in_descriptors() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "gemini.toml",
            "provider = \"gemini\"\napi_key = \"g\"\n\n[[models]]\nname = \"gemini-embedding-001\"\n\
             provider_model_id = \"gemini-embedding-001\"\ndim = 4\n",
        );
        let gateway = Gateway::load(dir.path(), &no_local());
        assert_eq!(gateway.models()[0]["provider"], "google");
    }

    #[tokio::test]
    async fn embeds_with_sub_batching_and_validates_shape() {
        // max_batch 2 over 5 texts → chunks of 2, 2, 1 → three requests.
        let response2 =
            r#"{"data":[{"embedding":[1.0,2.0],"index":0},{"embedding":[3.0,4.0],"index":1}]}"#;
        let response1 = r#"{"data":[{"embedding":[9.0,10.0],"index":0}]}"#;
        let m = mock::spawn(vec![
            (200, response2.to_string()),
            (200, response2.to_string()),
            (200, response1.to_string()),
        ])
        .await;

        let dir = private_tempdir();
        write_provider(dir.path(), "openai.toml", &openai_toml(&m.url, 2, 2));
        let gateway = Gateway::load(dir.path(), &no_local());

        let texts: Vec<String> = (0..5).map(|i| format!("text {i}")).collect();
        let out = gateway
            .embed(
                "openai-text-embedding-3-small",
                &texts,
                InputType::Document,
                far_deadline(),
            )
            .await
            .expect("embed ok");
        assert_eq!(out.len(), 5);
        assert_eq!(out[0], vec![1.0, 2.0]);
        assert_eq!(out[4], vec![9.0, 10.0]);
        assert_eq!(m.request_count(), 3, "sub-batched into three requests");
    }

    #[tokio::test]
    async fn wrong_count_and_wrong_dim_are_invalid_input_not_internal() {
        // One embedding for two inputs.
        let m = mock::always(200, r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#).await;
        let dir = private_tempdir();
        write_provider(dir.path(), "openai.toml", &openai_toml(&m.url, 2, 512));
        let gateway = Gateway::load(dir.path(), &no_local());

        let err = gateway
            .embed(
                "openai-text-embedding-3-small",
                &["a".to_string(), "b".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput, "{err}");

        // Right count, wrong dimension (3 instead of the declared 2).
        let m = mock::always(200, r#"{"data":[{"embedding":[1.0,2.0,3.0],"index":0}]}"#).await;
        let dir = private_tempdir();
        write_provider(dir.path(), "openai.toml", &openai_toml(&m.url, 2, 512));
        let gateway = Gateway::load(dir.path(), &no_local());

        let err = gateway
            .embed(
                "openai-text-embedding-3-small",
                &["a".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput, "{err}");
        assert!(err.message.contains("3-dim"), "{err}");
    }

    /// Right count, right dimensions, wrong *alignment*: a response that
    /// names input 0 twice would otherwise write one row's vector onto
    /// another row, permanently and undetectably. The count and dim guards
    /// both pass here — only the `text_index` check catches it.
    #[tokio::test]
    async fn a_response_that_is_not_row_parallel_is_refused() {
        let m = mock::always(
            200,
            r#"{"data":[{"embedding":[1.0,2.0],"index":0},{"embedding":[3.0,4.0],"index":0}]}"#,
        )
        .await;
        let dir = private_tempdir();
        write_provider(dir.path(), "openai.toml", &openai_toml(&m.url, 2, 512));
        let gateway = Gateway::load(dir.path(), &no_local());

        let err = gateway
            .embed(
                "openai-text-embedding-3-small",
                &["a".to_string(), "b".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidInput, "{err}");
        assert!(err.message.contains("row-parallel"), "{err}");
    }

    /// The normative §6.4 mapping table, exercised end to end against the
    /// mock server. Transient cases get a short deadline so the in-client
    /// retry cannot fit another attempt and the real error surfaces fast.
    #[tokio::test]
    async fn the_error_mapping_table_is_normative() {
        let cases: Vec<(u16, &str, ErrorCode)> = vec![
            (401, r#"{"error":"bad key"}"#, ErrorCode::UpstreamAuthFailed),
            (403, r#"{"error":"revoked"}"#, ErrorCode::UpstreamAuthFailed),
            (
                429,
                r#"{"error":"rate limited"}"#,
                ErrorCode::UpstreamServiceUnavailable,
            ),
            (
                503,
                r#"{"error":"down"}"#,
                ErrorCode::UpstreamServiceUnavailable,
            ),
            (
                400,
                r#"{"error":"maximum context length is 8192 tokens"}"#,
                ErrorCode::ContextLengthExceeded,
            ),
            // Bedrock's ModelTimeoutException / ModelErrorException: the
            // model failed to answer, so the rows are not poison.
            (
                408,
                r#"{"message":"ModelTimeoutException"}"#,
                ErrorCode::UpstreamServiceUnavailable,
            ),
            (
                424,
                r#"{"message":"ModelErrorException"}"#,
                ErrorCode::UpstreamServiceUnavailable,
            ),
            // 413 says "too big" with no wording to interpret: bisection,
            // not a dead letter, and not conditional on English phrasing.
            (
                413,
                r#"{"message":"Request Entity Too Large"}"#,
                ErrorCode::ContextLengthExceeded,
            ),
            // A request-side 4xx is attributed to the rows until bisection
            // proves otherwise: providers refuse a whole request over one
            // bad item, and dead-lettering the batch would destroy healthy
            // neighbours for it.
            (
                400,
                r#"{"error":"malformed input"}"#,
                ErrorCode::ContextLengthExceeded,
            ),
            (
                404,
                r#"{"error":{"code":"model_not_found"}}"#,
                ErrorCode::ModelNotFound,
            ),
            (200, "garbage that is not json", ErrorCode::InvalidInput),
        ];
        for (status, body, expected) in cases {
            let m = mock::always(status, body).await;
            let dir = private_tempdir();
            write_provider(dir.path(), "openai.toml", &openai_toml(&m.url, 2, 512));
            let gateway = Gateway::load(dir.path(), &no_local());

            let deadline = Instant::now() + Duration::from_millis(300);
            let err = gateway
                .embed(
                    "openai-text-embedding-3-small",
                    &["x".to_string()],
                    InputType::Document,
                    deadline,
                )
                .await
                .unwrap_err();
            assert_eq!(err.code, expected, "status {status} body {body}: {err}");
        }

        // Network error → UpstreamServiceUnavailable.
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = l.local_addr().unwrap();
            drop(l);
            format!("http://{addr}")
        };
        let dir = private_tempdir();
        write_provider(dir.path(), "openai.toml", &openai_toml(&closed, 2, 512));
        let gateway = Gateway::load(dir.path(), &no_local());
        let err = gateway
            .embed(
                "openai-text-embedding-3-small",
                &["x".to_string()],
                InputType::Document,
                Instant::now() + Duration::from_millis(300),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::UpstreamServiceUnavailable, "{err}");

        // Exhausted deadline → Timeout, before any request is made.
        let err = gateway
            .embed(
                "openai-text-embedding-3-small",
                &["x".to_string()],
                InputType::Document,
                Instant::now() - Duration::from_millis(1),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout, "{err}");

        // Unowned model → ModelNotFound.
        let err = gateway
            .embed(
                "not-served",
                &["x".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ModelNotFound, "{err}");
    }

    /// A truncated 200 body is an outage, not poison. reqwest reports it
    /// with the same error kind as unparseable JSON, so this is the case
    /// that decides whether a reset connection retries or dead-letters a
    /// batch of rows. It must be Transient.
    #[tokio::test]
    async fn a_truncated_response_body_is_transient_not_a_dead_letter() {
        let m = mock::truncated(r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#).await;
        let dir = private_tempdir();
        write_provider(dir.path(), "openai.toml", &openai_toml(&m.url, 2, 512));
        let gateway = Gateway::load(dir.path(), &no_local());

        let err = gateway
            .embed(
                "openai-text-embedding-3-small",
                &["x".to_string()],
                InputType::Document,
                // Short: no in-client retry fits, so the first failure is
                // the one that surfaces.
                Instant::now() + Duration::from_millis(300),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::UpstreamServiceUnavailable, "{err}");
    }

    #[tokio::test]
    async fn cohere_query_purpose_selects_the_query_backend() {
        let body = r#"{"embeddings":[[0.1,0.2]]}"#;
        let m = mock::always(200, body).await;
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "cohere.toml",
            &format!(
                "provider = \"cohere\"\napi_key = \"co\"\nbase_url = \"{}\"\n\n[[models]]\n\
                 name = \"cohere-embed-v4-0\"\nprovider_model_id = \"embed-v4.0\"\ndim = 2\n",
                m.url
            ),
        );
        let gateway = Gateway::load(dir.path(), &no_local());

        gateway
            .embed(
                "cohere-embed-v4-0",
                &["q".to_string()],
                InputType::Query,
                far_deadline(),
            )
            .await
            .expect("embed ok");
        assert!(
            m.last_request().contains("\"input_type\":\"search_query\""),
            "{}",
            m.last_request()
        );

        gateway
            .embed(
                "cohere-embed-v4-0",
                &["d".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .expect("embed ok");
        assert!(
            m.last_request()
                .contains("\"input_type\":\"search_document\""),
            "{}",
            m.last_request()
        );
    }

    #[tokio::test]
    async fn the_max_concurrent_semaphore_bounds_the_call_within_the_deadline() {
        let m = mock::always(200, r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#).await;
        let dir = private_tempdir();
        write_provider(dir.path(), "openai.toml", &openai_toml(&m.url, 2, 512));
        let gateway = Gateway::load(dir.path(), &no_local());

        // Hold the provider's only permit: an embed with a short deadline
        // times out WAITING, without a request ever reaching the provider.
        let permits = gateway
            .permits_for_test("openai-text-embedding-3-small")
            .unwrap();
        let held = permits.clone().acquire_owned().await.unwrap();
        // The §7.3 budget is the CONFIGURED cap: it must not shrink while a
        // permit is in use.
        assert_eq!(gateway.inflight_budget(), 1, "budget ignores live permits");
        let err = gateway
            .embed(
                "openai-text-embedding-3-small",
                &["x".to_string()],
                InputType::Document,
                Instant::now() + Duration::from_millis(100),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Timeout, "{err}");
        assert_eq!(m.request_count(), 0, "no request while the permit is held");

        // Released: the same call succeeds.
        drop(held);
        gateway
            .embed(
                "openai-text-embedding-3-small",
                &["x".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .expect("embed ok after release");
        assert_eq!(m.request_count(), 1);
    }

    #[tokio::test]
    async fn reload_swaps_atomically_and_a_failed_reload_keeps_the_snapshot() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "openai.toml",
            &openai_toml("http://127.0.0.1:1", 4, 8),
        );
        let gateway = Gateway::load(dir.path(), &no_local());
        assert!(gateway.owns("openai-text-embedding-3-small"));

        // Structural failure (the path is a file, not a directory): Err,
        // and the previous snapshot still serves.
        let not_a_dir = dir.path().join("openai.toml");
        assert!(gateway.reload(&not_a_dir, &no_local()).is_err());
        assert!(
            gateway.owns("openai-text-embedding-3-small"),
            "failed reload keeps the previous snapshot"
        );

        // Successful reload of a different directory swaps the set.
        let other = private_tempdir();
        write_provider(
            other.path(),
            "mistral.toml",
            "provider = \"mistral\"\napi_key = \"mi\"\n\n[[models]]\nname = \"mistral-mistral-embed\"\n\
             provider_model_id = \"mistral-embed\"\ndim = 4\n",
        );
        let report = gateway.reload(other.path(), &no_local()).unwrap();
        assert_eq!((report.providers, report.models), (1, 1));
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert!(!gateway.owns("openai-text-embedding-3-small"));
        assert!(gateway.owns("mistral-mistral-embed"));

        // Reload to an empty (missing) directory: back to zero-config.
        let report = gateway
            .reload(&other.path().join("missing"), &no_local())
            .unwrap();
        assert_eq!(report.models, 0);
        assert!(gateway.is_empty());
    }

    #[tokio::test]
    async fn a_broken_provider_file_is_isolated_from_the_rest() {
        let dir = private_tempdir();
        write_provider(dir.path(), "aaa-broken.toml", "provider = 42");
        write_provider(
            dir.path(),
            "openai.toml",
            &openai_toml("http://127.0.0.1:1", 4, 8),
        );
        // Unsupported connector type: isolated at gateway build.
        write_provider(
            dir.path(),
            "zzz-unknown.toml",
            "provider = \"frobnicator\"\napi_key = \"k\"\n\n[[models]]\nname = \"frob-1\"\n\
             provider_model_id = \"f\"\ndim = 4\n",
        );

        let gateway = Gateway::empty();
        let report = gateway.reload(dir.path(), &no_local()).unwrap();
        assert_eq!(report.models, 1, "the good provider still serves");
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert!(gateway.owns("openai-text-embedding-3-small"));
        assert!(!gateway.owns("frob-1"));
    }

    /// A provider file claiming a name the engine already serves is refused
    /// at load, not shadowed.
    ///
    /// Shadowing was deterministic — local wins — but it left a *dormant*
    /// provider entry behind the local model, and unloading or deactivating
    /// that local model made the entry start serving: a bound column's source
    /// text began leaving the host with no `enable()` NOTICE and no `provider
    /// add` acknowledgement, because neither ran. Refusing keeps the same
    /// local-by-default outcome and deletes the dormant state rather than
    /// scheduling it.
    #[tokio::test]
    async fn a_name_the_engine_already_serves_is_refused_at_load() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "openai.toml",
            "provider = \"openai\"\napi_key = \"sk\"\n\n[[models]]\n\
             name = \"baai-bge-m3\"\nprovider_model_id = \"pretend\"\ndim = 4\n\n\
             [[models]]\nname = \"openai-text-embedding-3-small\"\n\
             provider_model_id = \"text-embedding-3-small\"\ndim = 8\n",
        );

        // Without a local model of that name the file serves normally.
        let gateway = Gateway::load(dir.path(), &no_local());
        assert!(gateway.owns("baai-bge-m3"));

        // With one, the whole file is refused — the provider is a unit, and
        // half-serving it would leave the same dormant entry behind.
        let local: std::collections::BTreeSet<String> = ["baai-bge-m3".to_string()].into();
        let gateway = Gateway::empty();
        let report = gateway.reload(dir.path(), &local).unwrap();
        assert_eq!(report.models, 0);
        assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
        assert!(
            report.errors[0].contains("already served by a local engine model"),
            "{:?}",
            report.errors
        );
        assert!(!gateway.owns("openai-text-embedding-3-small"));
    }
}
