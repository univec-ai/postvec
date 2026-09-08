//! The provider gateway (feature `wire`): the single implementation both
//! inference hosts mount — the embedded engine host inside postvec.so and
//! postvec-server.
//!
//! It owns the loaded `providers.d` state behind an `Arc` swap: `reload`
//! builds a complete new snapshot and swaps it in atomically; in-flight
//! `embed` calls hold the previous `Arc` and finish against it, and a failed
//! reload keeps the previous snapshot (never half-applies). Every provider
//! failure leaves here as a [`GatewayError`] carrying a `shared::ErrorCode`
//! (the wire vocabulary the extension already classifies).

use crate::config::{self, LoadOutcome, ModelDescriptor, ModelKind};
use crate::routing::{self, Effective, RouteInput};
use crate::{
    new_conversion_backend, new_embedding_backend, ConversionBackend, EmbeddingBackend,
    EmbeddingError,
};
use shared::ErrorCode;
use std::collections::{BTreeMap, BTreeSet};
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
    /// provider path (the hosts bypass their engine gates for it).
    permits: Arc<Semaphore>,
    /// The configured cap behind `permits` — `available_permits()` shrinks
    /// while embeds are in flight, so budget math must never read it.
    max_concurrent: usize,
    /// What this entry can do, per the descriptor's `kind`.
    backends: Backends,
    /// The OUTPUT dimension: an embed model's width, a converter's
    /// target-space width. Every returned vector is validated against it.
    dim: u32,
    max_batch: usize,
    /// Nested HubModel descriptor served in `/config`.
    descriptor: serde_json::Value,
}

/// The kind-specific half of a [`ModelEntry`]. One namespace, one map — a
/// converter entry passes through every rule an embed entry does (name
/// collisions, ceilings, the local-name reserve, per-provider permits); only
/// the call it can serve differs.
enum Backends {
    Embed {
        document: Box<dyn EmbeddingBackend>,
        /// Present only where purpose changes the request (Cohere, Gemini,
        /// UniVec); everything else serves queries through the document
        /// backend.
        query: Option<Box<dyn EmbeddingBackend>>,
    },
    Convert {
        backend: Box<dyn ConversionBackend>,
        /// The declared INPUT width. Every input vector is checked against
        /// it before a paid call is made (`ModelEntry::dim` is the output's).
        source_dim: u32,
    },
}

impl ModelEntry {
    /// The embedding backend for a purpose — `None` for a converter entry.
    fn embed_backend(&self, input_type: InputType) -> Option<&dyn EmbeddingBackend> {
        match &self.backends {
            Backends::Embed { document, query } => Some(match input_type {
                InputType::Query => query.as_deref().unwrap_or(document.as_ref()),
                InputType::Document => document.as_ref(),
            }),
            Backends::Convert { .. } => None,
        }
    }
}

#[derive(Default)]
struct Inner {
    models: BTreeMap<String, ModelEntry>,
}

/// The nested HubModel shape `discovery::parse_config` requires. A
/// flat object would be silently dropped by the parser — the round-trip
/// tests below and in the host pin this.
fn descriptor_json(
    provider_type: &str,
    provider_name: &str,
    endpoint: &str,
    model: &ModelDescriptor,
    priority: Option<&Effective>,
) -> serde_json::Value {
    let params = match model.kind {
        ModelKind::Embed => {
            let mut params = serde_json::json!({
                "model_type": "embed",
                "target_model": model.space_name(),
                "target_dim": model.dim,
            });
            if let Some(max_tokens) = model.max_tokens {
                params["sequence_len"] = serde_json::json!(max_tokens);
            }
            params
        }
        // The exact shape a LOCAL converter's `/config` entry has —
        // `source_model`/`target_model` in the resolver's (postvec public
        // name) vocabulary — so `postvec.models` caches this row as an
        // ordinary `model_type = 'convert'` and `resolve_convert()` selects
        // it with no special case. The provider-side pair travels only in
        // the top-level identity fields below.
        ModelKind::Convert => serde_json::json!({
            "model_type": "convert",
            "source_model": model.source_model,
            "target_model": model.target_model,
            "source_dim": model.source_dim,
            "target_dim": model.dim,
        }),
    };
    let mut value = serde_json::json!({
        "name": model.name,
        "status": "provider",
        "provider": provider_type,
        // The routing identity behind the public name, secret-free: the file
        // stem an operator administers and the id the provider's API is
        // asked for. A provider-backed model has no on-disk descriptor to
        // compare, so without these two a fleet check can only compare
        // *names* — and two nodes serving `openai-text-embedding-3-small`
        // from different files, model ids or dimensions look identical while
        // round-robin hands a caller vectors from either. For a converter
        // the id is the derived ROUTE id (`ModelDescriptor::route_model_id`),
        // which folds in the provider-side pair, the source dimension and
        // the postvec-side pair — same drift semantics, one derivation.
        "provider_file": provider_name,
        "provider_model_id": model.route_model_id(),
        // Region and base_url, digested — the rest of "where does this go".
        "provider_endpoint": endpoint,
        "configuration": {
            "name": model.name,
            "enabled": true,
            "params": params,
        }
    });
    if let Some(priority) = priority {
        value["priority"] = serde_json::json!(priority.priority);
        value["priority_explicit"] = serde_json::json!(priority.explicit);
    }
    value
}

impl Inner {
    /// Build a snapshot from a scanned directory. Provider-level factory
    /// failures are isolated to that provider, mirroring the per-file rule.
    fn build(
        outcome: LoadOutcome,
        local_models: &BTreeMap<String, Option<u32>>,
    ) -> (Inner, Vec<String>) {
        let mut errors: Vec<String> = outcome.errors.iter().map(|e| e.to_string()).collect();
        let mut inner = Inner::default();

        let mut accepted = Vec::new();
        for provider in outcome.providers {
            if let Some(clash) = provider
                .models
                .iter()
                .find(|m| local_models.contains_key(&m.name))
            {
                errors.push(format!(
                    "provider {:?}: model {:?} is already served by a local engine model; a \
                     public name must have exactly one owner. Rename the [[models]] entry — \
                     leaving it would let the provider take the name over the moment the \
                     local model is unloaded",
                    provider.name, clash.name
                ));
                continue;
            }
            accepted.push(provider);
        }

        let mut space_dim: BTreeMap<String, (u32, String)> = BTreeMap::new();
        for (name, dim) in local_models {
            if let Some(dim) = dim {
                space_dim.insert(name.clone(), (*dim, name.clone()));
            }
        }
        let mut skipped: BTreeSet<(String, String)> = BTreeSet::new();
        for provider in &accepted {
            for model in &provider.models {
                if model.kind != ModelKind::Embed {
                    continue;
                }
                let space = model.space_name();
                if let Some((dim, other)) = space_dim.get(space) {
                    if *dim != model.dim {
                        errors.push(format!(
                            "provider {:?}: model {:?} (dim {}) disagrees with {:?} (dim {dim}) \
                             on space {space:?}; not serving {:?}",
                            provider.name, model.name, model.dim, other, model.name
                        ));
                        skipped.insert((provider.name.clone(), model.name.clone()));
                    }
                } else {
                    space_dim.insert(space.to_string(), (model.dim, model.name.clone()));
                }
            }
        }

        let mut route_inputs = Vec::new();
        for name in local_models.keys() {
            route_inputs.push(RouteInput {
                name,
                space: name,
                kind: ModelKind::Embed,
                local: true,
                priority: None,
                added: None,
                file_stem: "",
            });
        }
        for provider in &accepted {
            for model in &provider.models {
                if skipped.contains(&(provider.name.clone(), model.name.clone())) {
                    continue;
                }
                route_inputs.push(RouteInput {
                    name: &model.name,
                    space: model.space_name(),
                    kind: model.kind,
                    local: false,
                    priority: model.priority,
                    added: model.added.as_deref(),
                    file_stem: &provider.name,
                });
            }
        }
        let priorities: BTreeMap<String, Effective> = routing::effective_priorities(&route_inputs)
            .into_iter()
            .map(|e| (e.name.clone(), e))
            .collect();

        'providers: for provider in accepted {
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
            let endpoint = config::endpoint_digest(
                provider.config.region.as_deref(),
                provider.config.base_url.as_deref(),
            );

            let mut entries = Vec::with_capacity(provider.models.len());
            for model in &provider.models {
                if skipped.contains(&(provider.name.clone(), model.name.clone())) {
                    continue;
                }
                let backends = match model.kind {
                    ModelKind::Embed => {
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
                                // A factory failure is configuration-shaped
                                // (bad provider type, missing credential
                                // field): it affects every model in the file
                                // identically, so skip the provider as a
                                // unit.
                                errors.push(format!("provider {:?}: {e}", provider.name));
                                continue 'providers;
                            }
                        };
                        // Purpose changes the request body for Cohere
                        // (`input_type`), Gemini (`taskType`) and UniVec
                        // (`input_type` on the OpenAI-compatible endpoint);
                        // one backend per purpose keeps it a constructor
                        // concern (the copied client's shape) instead of a
                        // per-call trait parameter.
                        //
                        // Every purpose-aware connector belongs in this list.
                        // Adding `taskType` to the Gemini client without
                        // adding it here meant every Gemini query was still
                        // embedded as `RETRIEVAL_DOCUMENT` — the parameter
                        // was built and never reached, which is worse than
                        // not having it, because the descriptor claims the
                        // distinction is honoured.
                        let query =
                            if matches!(provider_type.as_str(), "cohere" | "google" | "univec") {
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
                        Backends::Embed { document, query }
                    }
                    ModelKind::Convert => {
                        // Refused at load for any non-UniVec connector
                        // (`validate_converter_for_provider`), so these
                        // unwraps describe a file the loader accepted.
                        let backend = match new_conversion_backend(
                            &provider.config,
                            model.provider_source_id.as_deref().unwrap_or(""),
                            &model.provider_model_id,
                            model.dim,
                            Some(client.clone()),
                        ) {
                            Ok(backend) => backend,
                            Err(e) => {
                                errors.push(format!("provider {:?}: {e}", provider.name));
                                continue 'providers;
                            }
                        };
                        Backends::Convert {
                            backend,
                            source_dim: model.source_dim.unwrap_or(0),
                        }
                    }
                };
                entries.push((
                    model.name.clone(),
                    ModelEntry {
                        provider_name: provider.name.clone(),
                        permits: permits.clone(),
                        max_concurrent: provider.max_concurrent,
                        backends,
                        dim: model.dim,
                        max_batch: model.max_batch,
                        descriptor: descriptor_json(
                            &provider_type,
                            &provider.name,
                            &endpoint,
                            model,
                            priorities.get(&model.name),
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
    /// Tower ingress permits shared with the host: its own inflight cap
    /// plus this gateway's budget, widened (never narrowed) by reloads.
    ingress: Arc<Semaphore>,
    granted: std::sync::atomic::AtomicUsize,
}

impl Gateway {
    /// An empty gateway: owns nothing, serves nothing. The zero-config state.
    pub fn empty() -> Self {
        Gateway {
            inner: RwLock::new(Arc::new(Inner::default())),
            ingress: Arc::new(Semaphore::new(0)),
            granted: Default::default(),
        }
    }

    /// Local engine names as the load/reload reservation map (unknown dims).
    pub fn reserve(
        names: impl IntoIterator<Item = impl Into<String>>,
    ) -> BTreeMap<String, Option<u32>> {
        names.into_iter().map(|n| (n.into(), None)).collect()
    }

    /// Load a providers.d directory. Failures — structural or per-file —
    /// are logged and isolated; the returned gateway always exists and
    /// serves whatever loaded (possibly nothing). A missing directory is
    /// the ordinary zero-config case and logs nothing.
    pub fn load(dir: &Path, local_models: &BTreeMap<String, Option<u32>>) -> Self {
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
        local_models: &BTreeMap<String, Option<u32>>,
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
        let budget = self.inflight_budget();
        let granted = self
            .granted
            .fetch_max(budget, std::sync::atomic::Ordering::AcqRel);
        if budget > granted {
            self.ingress.add_permits(budget - granted);
        }
        Ok(report)
    }

    /// The host's ingress semaphore: `base` permits of its own plus the
    /// provider budget, kept in step with reloads. Call once per host.
    pub fn ingress(&self, base: usize) -> Arc<Semaphore> {
        self.ingress.add_permits(base);
        self.ingress.clone()
    }

    fn snapshot(&self) -> Arc<Inner> {
        self.inner.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Does this gateway serve `model`? The hosts consult this BEFORE
    /// `is_model_ready` — a provider name is never in the engine.
    pub fn owns(&self, model: &str) -> bool {
        self.snapshot().models.contains_key(model)
    }

    /// Does this gateway serve `model` as a CONVERTER? The hosts' dispatch
    /// for `ConvertEmbeddings` consults this — a provider-backed *embed*
    /// model cannot convert, and answering `MODEL_NOT_LOADED` for it (the
    /// engine's answer for every name it does not hold) keeps the two
    /// dispatch paths the same shape.
    pub fn owns_converter(&self, model: &str) -> bool {
        matches!(
            self.snapshot().models.get(model).map(|e| &e.backends),
            Some(Backends::Convert { .. })
        )
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

    /// Sum of the per-provider `max_concurrent` caps: the
    /// The sum of per-provider `max_concurrent`, mirrored into [`Gateway::ingress`].
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

    /// Nested HubModel descriptors for the `/config` merge, in name order.
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

        // The failures that belong to **one row** and that a provider reports
        // as a failure of the whole request. Both are detectable here, with
        // no round trip and no guessing: an empty string (OpenAI documents
        // `input` as required to be non-empty) and one containing a NUL
        // (rejected outright). `[good, "", good]` would otherwise come back
        // as a single 400 and dead-letter three rows for one bad one.
        //
        // PoisonRow hands them to the queue's bisection, which halves the
        // batch until the offending row is alone. That is safe *here*
        // precisely because the classification is evidence-based: bisection
        // converges on one row rather than walking the whole tree, which is
        // what a guess on a generic 4xx would do (see `map_embedding_error`).
        let culprit = texts
            .iter()
            .position(|text| text.is_empty())
            .map(|at| (at, "is empty"))
            .or_else(|| {
                texts
                    .iter()
                    .position(|text| text.contains('\0'))
                    .map(|at| (at, "contains a NUL byte"))
            });
        if let Some((position, why)) = culprit {
            return Err(GatewayError::new(
                ErrorCode::ContextLengthExceeded,
                format!(
                    "input {position} {why}, which providers reject for the whole request; \
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
            let backend = entry.embed_backend(input_type).ok_or_else(|| {
                // A converter's public name on an EmbedTexts call. Permanent:
                // the resolver never routes an embed here (`model_type =
                // 'convert'` rows are excluded from embed resolution), so
                // this is a caller naming the wrong model, not an outage.
                GatewayError::new(
                    ErrorCode::InvalidInput,
                    format!("{model:?} is a provider-backed converter, not an embedding model"),
                )
            })?;
            let embeddings = backend
                .embed(&refs, Some(deadline))
                .await
                .map_err(|e| map_embedding_error(&entry.provider_name, &e))?;

            // Response-shape contract: systematic count or dimension
            // mismatch is InvalidInput (Permanent), never InternalError.
            // InternalError is Transient and would burn the queue's retry
            // budget before dead-lettering every row.
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

    /// Convert `embeddings` with a provider-backed converter. The convert
    /// sibling of [`Gateway::embed`]: sub-batches by the descriptor's
    /// `max_batch`, enforces the same per-provider `max_concurrent`
    /// semaphore (a provider file's budget covers both call kinds), and
    /// validates count and dimension on every response.
    ///
    /// Inputs are validated against the declared `source_dim` — and for
    /// finiteness — BEFORE any paid call: a wrong-width or non-finite vector
    /// is a caller error the provider would bill us to refuse.
    pub async fn convert(
        &self,
        model: &str,
        embeddings: &[Vec<f32>],
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
        let Backends::Convert {
            backend,
            source_dim,
        } = &entry.backends
        else {
            return Err(GatewayError::new(
                ErrorCode::InvalidInput,
                format!("{model:?} is a provider-backed embedding model, not a converter"),
            ));
        };

        for (position, vector) in embeddings.iter().enumerate() {
            if vector.len() as u32 != *source_dim {
                return Err(GatewayError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "input {position} has {} components; {model:?} converts from a \
                         {source_dim}-dimensional space",
                        vector.len()
                    ),
                ));
            }
            if vector.iter().any(|component| !component.is_finite()) {
                return Err(GatewayError::new(
                    ErrorCode::InvalidInput,
                    format!("input {position} contains a non-finite component"),
                ));
            }
        }

        let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(embeddings.len());
        for chunk in embeddings.chunks(entry.max_batch.max(1)) {
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

            let converted = backend
                .convert(chunk, Some(deadline))
                .await
                .map_err(|e| map_embedding_error(&entry.provider_name, &e))?;

            // Response-shape contract: systematic count or dimension
            // mismatch is InvalidInput (Permanent), never InternalError.
            // Conversion responses carry no per-item index field, so order
            // IS the alignment contract — count plus the per-vector width
            // check below is everything the wire offers to assert.
            if converted.len() != chunk.len() {
                return Err(GatewayError::new(
                    ErrorCode::InvalidInput,
                    format!(
                        "provider {:?} returned {} vectors for {} inputs",
                        entry.provider_name,
                        converted.len(),
                        chunk.len()
                    ),
                ));
            }
            for vector in converted {
                if vector.len() as u32 != entry.dim {
                    return Err(GatewayError::new(
                        ErrorCode::InvalidInput,
                        format!(
                            "provider {:?} returned a {}-dim vector for {model:?} \
                             (declared dim {})",
                            entry.provider_name,
                            vector.len(),
                            entry.dim
                        ),
                    ));
                }
                vectors.push(vector);
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

/// Error mapping table. `provider` is the operator-facing
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
        // 402 is here because of UniVec, which answers PAYMENT_REQUIRED when
        // the key's account is out of credit — the same shape of problem as
        // a revoked key (fix the account, not the rows), and never a row's
        // fault.
        EmbeddingError::Api {
            status: 401..=403, ..
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
        // Any other 4xx, and the clients' 200-decode-failure sentinel:
        // permanent for this input/response shape.
        //
        // A previous pass sent every request-side 4xx to PoisonRow so that
        // bisection could isolate a bad row, on the arithmetic that a
        // batch-wide fault would cost a bounded `log2(n)` extra calls. That
        // was simply wrong: `bisect_embed` recurses into **both** halves when
        // a batch fails, so a request-wide refusal walks the whole tree —
        // `2n-1` calls — and each recursive call starts a *fresh*
        // `timeout_ms`. Guessing "maybe one row did this" against a provider
        // that is already refusing us is an amplification path, not a
        // recovery.
        //
        // The two failures that genuinely belong to one row are caught
        // precisely instead, and before any request: an empty input and one
        // containing a NUL, both of which providers reject for the whole
        // batch. A token-limit refusal arrives as `InputTooLong`. What is
        // left has no evidence of being row-attributable, so it dead-letters
        // with a message naming the status and `postvec.retry_dead()`
        // re-drives it once the cause is fixed.
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
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    /// A providers.d is 0700; `tempfile::tempdir()` honours the umask.
    /// No local engine model in scope: the ordinary case for these tests.
    fn no_local() -> BTreeMap<String, Option<u32>> {
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

    /// Nested HubModel shape, asserted field by field - the crate-side twin of the
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
        assert_eq!(m["priority"], 200);
        assert_eq!(m["priority_explicit"], false);
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
            // A real width for this model: the loader validates `dim`
            // against Gemini's documented 128..=3072 range.
            "provider = \"gemini\"\napi_key = \"g\"\n\n[[models]]\nname = \"gemini-embedding-001\"\n\
             provider_model_id = \"gemini-embedding-001\"\ndim = 3072\n",
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

    /// Error mapping table, exercised end to end against the
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
            // A generic 4xx dead-letters: there is no evidence it belongs
            // to any one row, and bisecting on a guess walks the whole tree
            // (2n-1 calls, each with a fresh timeout).
            (
                400,
                r#"{"error":"malformed input"}"#,
                ErrorCode::InvalidInput,
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
        // The real width of `embed-english-v3.0`. A descriptor claiming
        // anything else is refused at load, so the mock has to answer with
        // what the model actually produces — a two-component "Cohere v3"
        // response is not a shape this test is allowed to assume.
        const DIM: usize = 1024;
        let vector = (0..DIM)
            .map(|i| format!("{}", i as f32 / DIM as f32))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(r#"{{"embeddings":{{"float":[[{vector}]]}}}}"#);
        let m = mock::always(200, &body).await;
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "cohere.toml",
            &format!(
                "provider = \"cohere\"\napi_key = \"co\"\nbase_url = \"{}\"\n\n[[models]]\n\
                 name = \"cohere-embed-english-v3-0\"\n\
                 provider_model_id = \"embed-english-v3.0\"\ndim = {DIM}\n",
                m.url
            ),
        );
        let gateway = Gateway::load(dir.path(), &no_local());

        gateway
            .embed(
                "cohere-embed-english-v3-0",
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
                "cohere-embed-english-v3-0",
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
        // The inflight budget is the configured cap: it must not shrink while a
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
        let local: BTreeMap<String, Option<u32>> = [("baai-bge-m3".to_string(), None)].into();
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

    /// A UniVec file with an embed model and a converter — the fixture the
    /// convert tests share. `dim` (output) is 2 and `source_dim` is 3, so a
    /// swapped-axis bug cannot pass both width checks by coincidence.
    fn univec_toml(base_url: &str) -> String {
        format!(
            "provider = \"univec\"\napi_key = \"uv-test\"\nbase_url = \"{base_url}\"\n\
             max_concurrent = 1\n\n\
             [[models]]\nname = \"univec-arctic\"\n\
             provider_model_id = \"snowflake-arctic-embed-l-v2.0\"\ndim = 2\n\n\
             [[models]]\nname = \"univec-convert-a-to-b\"\nkind = \"convert\"\n\
             provider_model_id = \"target-space\"\nprovider_source_id = \"source-space\"\n\
             source_model = \"model-a\"\ntarget_model = \"model-b\"\n\
             source_dim = 3\ndim = 2\nmax_batch = 2\n"
        )
    }

    /// The converter descriptor must be byte-compatible with a LOCAL
    /// converter's `/config` entry — that is what makes `resolve_convert()`
    /// select it with no special case — while its top-level identity carries
    /// the derived route id.
    #[tokio::test]
    async fn a_univec_converter_advertises_the_local_convert_shape() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "univec.toml",
            &univec_toml("http://127.0.0.1:1"),
        );
        let gateway = Gateway::load(dir.path(), &no_local());

        let models = gateway.models();
        assert_eq!(models.len(), 2, "{models:?}");
        let convert = models
            .iter()
            .find(|m| m["name"] == "univec-convert-a-to-b")
            .expect("converter descriptor");
        assert_eq!(convert["status"], "provider");
        assert_eq!(convert["provider"], "univec");
        let params = &convert["configuration"]["params"];
        assert_eq!(params["model_type"], "convert");
        assert_eq!(params["source_model"], "model-a");
        assert_eq!(params["target_model"], "model-b");
        assert_eq!(params["source_dim"], 3);
        assert_eq!(params["target_dim"], 2);
        assert!(params.get("sequence_len").is_none());
        // The route id folds in everything that decides what the route
        // means; the raw provider ids never stand alone.
        assert_eq!(
            convert["provider_model_id"],
            "source-space->target-space for model-a[3]->model-b"
        );

        assert!(gateway.owns("univec-convert-a-to-b"));
        assert!(gateway.owns_converter("univec-convert-a-to-b"));
        assert!(gateway.owns("univec-arctic"));
        assert!(!gateway.owns_converter("univec-arctic"));
    }

    #[tokio::test]
    async fn converts_with_sub_batching_and_validates_count() {
        // max_batch 2 over 3 vectors → chunks of 2, 1 → two requests.
        let response2 = r#"{"success":true,"data":{"embeddings":[[1.0,2.0],[3.0,4.0]],"usage":{"prompt_tokens":0,"total_tokens":0}}}"#;
        let response1 = r#"{"success":true,"data":{"embeddings":[[9.0,10.0]]}}"#;
        let m = mock::spawn(vec![
            (200, response2.to_string()),
            (200, response1.to_string()),
        ])
        .await;

        let dir = private_tempdir();
        write_provider(dir.path(), "univec.toml", &univec_toml(&m.url));
        let gateway = Gateway::load(dir.path(), &no_local());

        let inputs = vec![
            vec![0.1, 0.2, 0.3],
            vec![0.4, 0.5, 0.6],
            vec![0.7, 0.8, 0.9],
        ];
        let out = gateway
            .convert("univec-convert-a-to-b", &inputs, far_deadline())
            .await
            .expect("convert ok");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0], vec![1.0, 2.0]);
        assert_eq!(out[2], vec![9.0, 10.0]);
        assert_eq!(m.request_count(), 2, "sub-batched into two requests");
        // The provider-side pair travels on the wire; the postvec-side names
        // do not (they are the resolver's vocabulary, not UniVec's).
        let body = m.last_request();
        assert!(body.contains("\"source_model\":\"source-space\""), "{body}");
        assert!(body.contains("\"target_model\":\"target-space\""), "{body}");
    }

    /// A wrong-width or non-finite input is a caller error the provider
    /// would bill us to refuse — no request may be made for it.
    #[tokio::test]
    async fn bad_convert_inputs_are_refused_before_any_request() {
        let m = mock::always(200, r#"{"success":true,"data":{"embeddings":[[1.0,2.0]]}}"#).await;
        let dir = private_tempdir();
        write_provider(dir.path(), "univec.toml", &univec_toml(&m.url));
        let gateway = Gateway::load(dir.path(), &no_local());

        // Two components where the source space has three.
        let err = gateway
            .convert("univec-convert-a-to-b", &[vec![0.1, 0.2]], far_deadline())
            .await
            .expect_err("wrong width");
        assert_eq!(err.code, ErrorCode::InvalidInput);

        // Correct width, non-finite component.
        let err = gateway
            .convert(
                "univec-convert-a-to-b",
                &[vec![0.1, f32::NAN, 0.3]],
                far_deadline(),
            )
            .await
            .expect_err("non-finite");
        assert_eq!(err.code, ErrorCode::InvalidInput);

        assert_eq!(m.request_count(), 0, "no paid call was spent");
    }

    /// Each call kind refuses the other kind's models, permanently — the
    /// resolver never routes these, so reaching one is a caller naming the
    /// wrong model, not an outage to retry.
    #[tokio::test]
    async fn each_call_kind_refuses_the_other_kinds_models() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "univec.toml",
            &univec_toml("http://127.0.0.1:1"),
        );
        let gateway = Gateway::load(dir.path(), &no_local());

        let err = gateway
            .embed(
                "univec-convert-a-to-b",
                &["text".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .expect_err("a converter cannot embed");
        assert_eq!(err.code, ErrorCode::InvalidInput);

        let err = gateway
            .convert("univec-arctic", &[vec![0.0; 3]], far_deadline())
            .await
            .expect_err("an embed model cannot convert");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    /// UniVec is purpose-aware: a query goes out with `input_type`
    /// "search_query", a document with "search_document" — the same
    /// build-both-backends rule as Cohere and Gemini, pinned because adding
    /// the parameter to the client without adding the connector to the
    /// gateway's query list is exactly how Gemini's version of this broke.
    #[tokio::test]
    async fn univec_embeds_queries_and_documents_with_their_purpose() {
        let m = mock::always(200, r#"{"data":[{"embedding":[1.0,2.0],"index":0}]}"#).await;
        let dir = private_tempdir();
        write_provider(dir.path(), "univec.toml", &univec_toml(&m.url));
        let gateway = Gateway::load(dir.path(), &no_local());

        gateway
            .embed(
                "univec-arctic",
                &["a query".to_string()],
                InputType::Query,
                far_deadline(),
            )
            .await
            .expect("query embed");
        assert!(
            m.last_request().contains("\"input_type\":\"search_query\""),
            "{}",
            m.last_request()
        );

        gateway
            .embed(
                "univec-arctic",
                &["a document".to_string()],
                InputType::Document,
                far_deadline(),
            )
            .await
            .expect("document embed");
        assert!(
            m.last_request()
                .contains("\"input_type\":\"search_document\""),
            "{}",
            m.last_request()
        );
        // The descriptor's dim rides as `dimensions` (Matryoshka truncation).
        assert!(
            m.last_request().contains("\"dimensions\":2"),
            "{}",
            m.last_request()
        );
    }

    /// UniVec answers 402 when the key's account is out of credit: an ops
    /// problem exactly like a revoked key — Config extension-side, bounded
    /// retry and failover, never a dead-lettered row.
    #[tokio::test]
    async fn a_402_is_classified_with_the_auth_failures() {
        let m = mock::always(
            402,
            r#"{"success":false,"error":{"message":"API key quota exceeded"}}"#,
        )
        .await;
        let dir = private_tempdir();
        write_provider(dir.path(), "univec.toml", &univec_toml(&m.url));
        let gateway = Gateway::load(dir.path(), &no_local());

        let err = gateway
            .convert("univec-convert-a-to-b", &[vec![0.0; 3]], far_deadline())
            .await
            .expect_err("out of credit");
        assert_eq!(err.code, ErrorCode::UpstreamAuthFailed);
    }

    /// A 200 whose envelope says `success: false`, and a response with the
    /// wrong count or width, are permanent for this response — InvalidInput,
    /// never a transient that burns the retry budget.
    #[tokio::test]
    async fn convert_response_shape_violations_are_invalid_input() {
        // success: false with a data payload anyway.
        let m = mock::always(
            200,
            r#"{"success":false,"data":{"embeddings":[[1.0,2.0]]}}"#,
        )
        .await;
        let dir = private_tempdir();
        write_provider(dir.path(), "univec.toml", &univec_toml(&m.url));
        let gateway = Gateway::load(dir.path(), &no_local());
        let err = gateway
            .convert("univec-convert-a-to-b", &[vec![0.0; 3]], far_deadline())
            .await
            .expect_err("success=false");
        assert_eq!(err.code, ErrorCode::InvalidInput);

        // Two vectors for one input.
        let m = mock::always(
            200,
            r#"{"success":true,"data":{"embeddings":[[1.0,2.0],[3.0,4.0]]}}"#,
        )
        .await;
        write_provider(dir.path(), "univec.toml", &univec_toml(&m.url));
        let gateway = Gateway::load(dir.path(), &no_local());
        let err = gateway
            .convert("univec-convert-a-to-b", &[vec![0.0; 3]], far_deadline())
            .await
            .expect_err("count mismatch");
        assert_eq!(err.code, ErrorCode::InvalidInput);

        // Right count, wrong width (three components where dim is 2).
        let m = mock::always(
            200,
            r#"{"success":true,"data":{"embeddings":[[1.0,2.0,3.0]]}}"#,
        )
        .await;
        write_provider(dir.path(), "univec.toml", &univec_toml(&m.url));
        let gateway = Gateway::load(dir.path(), &no_local());
        let err = gateway
            .convert("univec-convert-a-to-b", &[vec![0.0; 3]], far_deadline())
            .await
            .expect_err("dim mismatch");
        assert_eq!(err.code, ErrorCode::InvalidInput);
    }

    #[tokio::test]
    async fn embed_descriptor_advertises_space_as_target_model() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "openrouter.toml",
            "provider = \"openrouter\"\napi_key = \"k\"\n\n\
             [[models]]\nname = \"openrouter-google-gemini-embedding-001\"\n\
             provider_model_id = \"google/gemini-embedding-001\"\ndim = 3072\n\
             space = \"gemini-embedding-001\"\npriority = 1\n\
             added = \"2026-09-08T00:00:00Z\"\n",
        );
        let gateway = Gateway::load(dir.path(), &no_local());
        let m = &gateway.models()[0];
        assert_eq!(
            m["configuration"]["params"]["target_model"],
            "gemini-embedding-001"
        );
        assert_eq!(m["name"], "openrouter-google-gemini-embedding-001");
        assert_eq!(m["priority"], 1);
        assert_eq!(m["priority_explicit"], true);
    }

    #[tokio::test]
    async fn a_provider_row_that_disagrees_with_a_local_space_dim_is_not_served() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "univec.toml",
            "provider = \"univec\"\napi_key = \"k\"\n\n\
             [[models]]\nname = \"univec-baai-bge-m3\"\n\
             provider_model_id = \"baai-bge-m3\"\ndim = 768\nspace = \"baai-bge-m3\"\n\
             [[models]]\nname = \"univec-other\"\n\
             provider_model_id = \"other\"\ndim = 8\n",
        );
        let local: BTreeMap<String, Option<u32>> = [("baai-bge-m3".to_string(), Some(1024))].into();
        let gateway = Gateway::empty();
        let report = gateway.reload(dir.path(), &local).unwrap();
        assert!(
            report.errors.iter().any(|e| e.contains("disagrees")
                && e.contains("univec-baai-bge-m3")
                && e.contains("baai-bge-m3")),
            "{:?}",
            report.errors
        );
        assert!(!gateway.owns("univec-baai-bge-m3"));
        assert!(gateway.owns("univec-other"));
    }

    #[tokio::test]
    async fn unstamped_provider_routes_sort_after_stamped_ones() {
        let dir = private_tempdir();
        write_provider(
            dir.path(),
            "a.toml",
            "provider = \"openai\"\napi_key = \"k\"\n\n\
             [[models]]\nname = \"stamped\"\nprovider_model_id = \"s\"\ndim = 8\n\
             space = \"s\"\nadded = \"2026-01-01T00:00:00Z\"\n",
        );
        write_provider(
            dir.path(),
            "b.toml",
            "provider = \"openai\"\napi_key = \"k\"\n\n\
             [[models]]\nname = \"unstamped\"\nprovider_model_id = \"u\"\ndim = 8\n\
             space = \"s\"\n",
        );
        let gateway = Gateway::load(dir.path(), &no_local());
        let models = gateway.models();
        let stamped = models.iter().find(|m| m["name"] == "stamped").unwrap();
        let unstamped = models.iter().find(|m| m["name"] == "unstamped").unwrap();
        assert_eq!(stamped["priority"], 200);
        assert_eq!(unstamped["priority"], 201);
        assert_eq!(stamped["priority_explicit"], false);
    }
}
