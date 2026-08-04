//! `postvec.embed` / `postvec.convert` / `postvec.refresh_models`:
//! synchronous one-shot inference (debugging / ad-hoc) plus the model-cache
//! refresh.
//!
//! SPI work (resolution, upsert) happens strictly before or after
//! `runtime::block_on_with_timeout`, never inside the future.

#[cfg(any(test, feature = "pg_test"))]
use crate::client::discovery;
use crate::client::discovery::DiscoveryReport;
use crate::client::grpc::GrpcClient;
use crate::client::{EmbedPurpose, EmbedRoute, InferenceClient, ModelInfo, PvError};
use crate::{gucs, runtime};
use pgrx::prelude::*;

pub(crate) fn query_timeout_ms() -> u64 {
    gucs::QUERY_TIMEOUT_MS.get().max(50) as u64
}

/// Refuse an input over the configured per-item byte ceiling before it is
/// cloned, rendered into a request or shipped over gRPC. PostgreSQL text
/// can be far larger than any model or transport can use; the cap has to
/// exist before the first allocation, not at the remote.
pub(crate) fn assert_input_within_cap(what: &str, len: usize) {
    let cap = crate::jobs::effective_item_cap() as usize;
    if len > cap {
        error!(
            "postvec: {what} is {len} bytes; the ceiling is {cap} bytes (the minimum of \
             postvec.max_document_bytes, postvec.max_batch_total_bytes, and the 48 MiB \
             wire-message ceiling)"
        );
    }
}

/// Validate-and-copy a borrowed PostgreSQL text array element by element.
/// The per-item and summed-batch byte ceilings run against the borrowed
/// array datum before any element is duplicated into Rust, so an over-cap
/// batch errors without being materialized twice.
pub(crate) fn collect_batch_within_caps(what: &str, inputs: pgrx::Array<'_, &str>) -> Vec<String> {
    let budget = gucs::MAX_BATCH_TOTAL_BYTES.get().max(65_536) as usize;
    let mut total = 0usize;
    let mut out = Vec::with_capacity(inputs.len());
    for item in inputs.iter() {
        let Some(t) = item else {
            error!("postvec: {what} batch contains a NULL element");
        };
        assert_input_within_cap(what, t.len());
        total = total.saturating_add(t.len());
        if total > budget {
            error!(
                "postvec: {what} batch exceeds {budget} summed bytes \
                 (postvec.max_batch_total_bytes); split the call"
            );
        }
        out.push(t.to_string());
    }
    out
}

/// The cached target dimension for a public model name — the embed model's
/// `target_dim`, or a converter's into that space — used to size response
/// sub-batches and the output budget. When the cache does not know, the
/// fallback is pgvector's 16,000-dimension CEILING (the true supported
/// maximum): assuming smaller only makes batches larger than the envelope,
/// so unknown must mean maximally conservative.
fn cached_target_dim(model_public: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "SELECT COALESCE(
            (SELECT target_dim FROM postvec.models
              WHERE model_type = 'embed' AND (target_model = $1 OR name = $1)
                AND target_dim IS NOT NULL
              ORDER BY (target_model = $1) IS TRUE DESC, name LIMIT 1),
            (SELECT target_dim FROM postvec.models
              WHERE model_type = 'convert' AND target_model = $1
                AND target_dim IS NOT NULL
              ORDER BY name LIMIT 1),
            16000)",
        &[model_public.into()],
    )
    .ok()
    .flatten()
    .unwrap_or(16_000)
}

/// Resolve + embed a batch of texts through the backend-side gRPC client,
/// bounded by `postvec.query_timeout_ms`. Shared by `embed()`, `enable()`'s
/// dimension probe, and `search()`'s query embedding. Falls back to an
/// embed-bridge route for models that are only reachable as a converter
/// target (see [`resolve_embed_route`]).
///
/// Sub-batched exactly like the worker paths: no single call's response may
/// exceed the wire budget for the target dimension, nor its request the
/// request budget — `embed(text[])` with 4,096 short texts against a
/// high-dimensional model would otherwise produce one response over the
/// transport's decode ceiling (and, embedded, a launcher-memory spike while
/// the engine builds the response tree). All sub-batches share ONE total
/// deadline.
pub(crate) fn embed_texts(
    texts: &[String],
    model_public: &str,
    purpose: EmbedPurpose,
) -> Result<Vec<Vec<f32>>, PvError> {
    let (model, route) = resolve_embed_route(model_public)?.into_call();
    let route = route.with_purpose(purpose);
    let timeout = query_timeout_ms();
    let client = GrpcClient::from_gucs(timeout);
    let overall = client.overall_timeout_ms();
    let dim = cached_target_dim(model_public);
    // Cap the TOTAL accumulated output (the caller receives every sub-batch's
    // vectors at once): items x dim x ~16 bytes per f32-with-Vec-overhead
    // must fit the output budget, or the call is refused up front.
    const OUTPUT_BUDGET_BYTES: u64 = 256 * 1024 * 1024;
    let max_out_items = (OUTPUT_BUDGET_BYTES / (dim.max(1) as u64 * 16)).max(1) as usize;
    if texts.len() > max_out_items {
        return Err(PvError::InvalidInput(format!(
            "{} embeddings at {dim} dims would materialize more than the {OUTPUT_BUDGET_BYTES}-byte \
             output budget; send at most {max_out_items} inputs per call",
            texts.len()
        )));
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(overall);
    let mut out = Vec::with_capacity(texts.len());
    for chunk in crate::jobs::request_chunks(texts, dim) {
        let remaining = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as u64;
        if remaining == 0 {
            return Err(PvError::Deadline { ms: overall });
        }
        let vecs = runtime::block_on_with_timeout(remaining, async {
            client.embed(chunk, &model, &route).await
        })?;
        out.extend(vecs);
    }
    Ok(out)
}

/// `Spi::get_one_with_args` returns `Err(InvalidPosition)` when the query
/// yields zero rows; for resolution lookups that simply means "not found".
fn spi_opt_string(
    query: &str,
    args: &[pgrx::datum::DatumWithOid<'_>],
) -> Result<Option<String>, PvError> {
    match Spi::get_one_with_args::<String>(query, args) {
        Ok(v) => Ok(v),
        Err(pgrx::spi::SpiError::InvalidPosition) => Ok(None),
        Err(e) => Err(PvError::Internal(format!("SPI: {e}"))),
    }
}

/// First-tier embed resolution: public `target_model` wins, internal name
/// accepted as fallback.
pub(crate) fn resolve_embed(model: &str) -> Result<String, PvError> {
    // `IS TRUE` folds NULL (models with no target_model) to false — plain
    // `DESC` is NULLS FIRST and would rank them above an exact match.
    let found = spi_opt_string(
        "SELECT name FROM postvec.models
          WHERE model_type = 'embed' AND (target_model = $1 OR name = $1)
          ORDER BY (target_model = $1) IS TRUE DESC, name
          LIMIT 1",
        &[model.into()],
    )?;
    found.ok_or_else(|| PvError::UnknownModel(model.to_string()))
}

/// How an embed request will be routed.
#[derive(Debug, PartialEq, Eq)]
pub enum EmbedResolution {
    /// A hosted embed model matches — embed directly (internal name).
    Direct(String),
    /// No embed model, but the target is reachable as a converter's
    /// `target_model` whose *source* is embeddable: route through an
    /// `embed-bridge` executor — texts are embedded with `via` and converted
    /// into `target`'s space engine-side, in one RPC. This is what lets a
    /// column live on a commercial model (e.g. a Cohere target) with no
    /// provider API key anywhere: fresh writes bridge through the local model.
    Bridge {
        /// Internal name of the embed-bridge executor (gRPC `model` field).
        executor: String,
        /// Semantic name of the model that embeds the text (`bridge_model`).
        via: String,
        /// Semantic name of the target space (`target_model`).
        target: String,
    },
}

impl EmbedResolution {
    /// The (gRPC model, route) pair for [`InferenceClient::embed`].
    pub fn into_call(self) -> (String, EmbedRoute) {
        match self {
            EmbedResolution::Direct(name) => (name, EmbedRoute::default()),
            EmbedResolution::Bridge {
                executor,
                via,
                target,
            } => (
                executor,
                EmbedRoute {
                    bridge_model: Some(via),
                    target_model: Some(target),
                    ..Default::default()
                },
            ),
        }
    }
}

/// Two-tier embed resolution: a direct embed model wins; otherwise fall back
/// to an embed-bridge route (embed with a converter's source model, convert
/// into the requested space). Deterministic: candidate converters are ranked
/// by name. Re-resolved from the cache on every use, so a model that later
/// becomes directly embeddable (e.g. a provider API key added on the inference side)
/// upgrades to `Direct` automatically.
pub(crate) fn resolve_embed_route(model: &str) -> Result<EmbedResolution, PvError> {
    match resolve_embed(model) {
        Ok(internal) => return Ok(EmbedResolution::Direct(internal)),
        Err(PvError::UnknownModel(_)) => {}
        Err(e) => return Err(e),
    }

    // Bridge tier: a converter targeting `model` whose source is itself a
    // resolvable embed output. Match the engine resolver exactly: target_model
    // is the semantic key when present; internal name is only its fallback.
    //
    // Direct embed wins. Otherwise take the lexicographically first
    // converter into the target whose source is embeddable. An entry-level
    // preference can replace this later without changing search()'s SQL
    // signature.
    //
    // Provider-backed rows are excluded from BOTH sides. The bridge goes on
    // the wire as one EmbedTexts naming the embed-bridge executor, and that
    // executor resolves its chain against the ENGINE's resolver — which
    // indexes engine-loaded models only, never the provider gateway's.
    // Counting a gateway-served converter (or a gateway-served source model)
    // here would resolve a route the host must then refuse on every call.
    let via = spi_opt_string(
        "SELECT c.source_model
           FROM postvec.models c
          WHERE c.model_type = 'convert' AND c.target_model = $1
            AND c.source_model IS NOT NULL
            AND c.raw->'extra'->>'provider' IS NULL
            AND EXISTS (SELECT 1 FROM postvec.models e
                         WHERE e.model_type = 'embed'
                           AND COALESCE(e.target_model, e.name) = c.source_model
                           AND e.raw->'extra'->>'provider' IS NULL)
          ORDER BY c.name LIMIT 1",
        &[model.into()],
    )?;
    let Some(via) = via else {
        // Failure path only: distinguish "no converter at all" from "a
        // converter exists but its source is not embeddable" — the operator
        // fix differs (publish a converter vs. host/enable its source model).
        let dead_converter = spi_opt_string(
            "SELECT c.name || ' (source ' || COALESCE(c.source_model, 'NULL') || ')'
               FROM postvec.models c
              WHERE c.model_type = 'convert' AND c.target_model = $1
                AND c.raw->'extra'->>'provider' IS NULL
              ORDER BY c.name LIMIT 1",
            &[model.into()],
        )?;
        let detail = match dead_converter {
            Some(desc) => format!(
                "no embed model matches; converter {desc} targets it but its source model \
                 is not itself embeddable"
            ),
            None => {
                // A provider-backed converter into this space is real but
                // cannot back an embed bridge; say so rather than "nothing
                // targets it" — the operator fix differs again
                // (migrate()/convert() work today; an embed route needs a
                // local converter or a direct provider model).
                let provider_converter = spi_opt_string(
                    "SELECT c.name FROM postvec.models c
                      WHERE c.model_type = 'convert' AND c.target_model = $1
                        AND c.raw->'extra'->>'provider' IS NOT NULL
                      ORDER BY c.name LIMIT 1",
                    &[model.into()],
                )?;
                match provider_converter {
                    Some(name) => format!(
                        "no embed model matches; provider-backed converter {name} targets it, \
                         but hosted converters serve postvec.migrate()/convert() only — an \
                         embed bridge runs inside the engine"
                    ),
                    None => "no embed model matches and no converter targets it".to_string(),
                }
            }
        };
        return Err(PvError::NoEmbedPath {
            model: model.to_string(),
            detail,
        });
    };
    let executor = spi_opt_string(
        "SELECT name FROM postvec.models
          WHERE model_type = 'embed-bridge' ORDER BY name LIMIT 1",
        &[],
    )?;
    let Some(executor) = executor else {
        return Err(PvError::NoEmbedPath {
            model: model.to_string(),
            detail: format!(
                "a converter from {via:?} exists but no embed-bridge executor is available"
            ),
        });
    };
    Ok(EmbedResolution::Bridge {
        executor,
        via,
        target: model.to_string(),
    })
}

/// Resolve a conversion to the DIRECT converter's name — local or
/// provider-backed, either serves under its own name on the wire.
///
/// Deliberately nothing else: the two-hop path through the `convert-bridge`
/// executor was removed ahead of that executor's deprecation. A cached
/// `convert-bridge` row (a remote node may still advertise one) is inert
/// here, and a chain that would need one is `NoConvertPath` — the fix is a
/// direct converter (or `strategy => 'reembed'`), not a chain.
/// A local converter wins over a provider-backed one for the same pair
/// (`raw->'extra'->>'provider'` is what discovery stamps on gateway rows):
/// a migration must not silently pick the billable, vector-egress route
/// over a converter the operator pulled to disk. Then lexical order.
pub(crate) fn resolve_convert(source: &str, target: &str) -> Result<String, PvError> {
    let direct = spi_opt_string(
        "SELECT name FROM postvec.models
          WHERE model_type = 'convert' AND source_model = $1 AND target_model = $2
          ORDER BY (raw->'extra'->>'provider') IS NOT NULL, name LIMIT 1",
        &[source.into(), target.into()],
    )?;
    direct.ok_or_else(|| PvError::NoConvertPath {
        from: source.to_string(),
        to: target.to_string(),
    })
}

/// Fetch models from all configured HTTP endpoints (network; no SPI). Shared
/// by the `refresh_models()` SQL function and the worker's refresh sub-loop.
/// Nodes are polled in 4-wide waves (`DISCOVERY_CONCURRENCY`), each request
/// bounded by `postvec.discovery_timeout_ms`, so a full refresh costs at
/// most `per-node timeout × ceil(endpoints/4) + 1 s`
/// (`GrpcClient::discovery_waves`), not one timeout per node.
pub(crate) fn list_models_report_blocking() -> Result<DiscoveryReport, PvError> {
    let per_node = gucs::DISCOVERY_TIMEOUT_MS.get().max(100) as u64;
    let client = GrpcClient::from_gucs(query_timeout_ms());
    // Multi-wave budget: fan-out is bounded at DISCOVERY_CONCURRENCY
    // nodes, so E endpoints take up to ceil(E/4) back-to-back per-node
    // timeouts. A flat per-node+1s budget would abort the whole refresh
    // (discarding every completed node's result) whenever slow leading
    // endpoints pushed a healthy later node into the next wave.
    let budget = per_node
        .saturating_mul(client.discovery_waves())
        .saturating_add(1_000);
    runtime::block_on_with_timeout(budget, async { client.list_models_report().await })
}

/// Upsert discovered models into the cache in one set-based, diff-aware
/// statement. A model row whose material fields are unchanged is not
/// rewritten, so a periodic refresh of a stable fleet produces no row
/// versions and no WAL. `models.last_seen` only moves when a row actually
/// changes; cache freshness is recorded on
/// `worker_heartbeat.models_refreshed_at`. Rows for vanished models are
/// kept. Must be called inside a transaction.
pub(crate) fn upsert_models(models: &[ModelInfo]) -> Result<(), pgrx::spi::Error> {
    if models.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
    let types: Vec<String> = models.iter().map(|m| m.model_type.clone()).collect();
    let sources: Vec<Option<String>> = models.iter().map(|m| m.source_model.clone()).collect();
    let targets: Vec<Option<String>> = models.iter().map(|m| m.target_model.clone()).collect();
    let source_dims: Vec<Option<i32>> = models
        .iter()
        .map(|m| m.source_dim.map(|d| d as i32))
        .collect();
    let target_dims: Vec<Option<i32>> = models
        .iter()
        .map(|m| m.target_dim.map(|d| d as i32))
        .collect();
    let seq_lens: Vec<Option<i32>> = models
        .iter()
        .map(|m| m.sequence_len.map(|d| d as i32))
        .collect();
    let raws: Vec<String> = models.iter().map(|m| m.raw.to_string()).collect();
    Spi::connect_mut(|client| {
        client.update(
            "INSERT INTO postvec.models
               (name, model_type, source_model, target_model,
                source_dim, target_dim, sequence_len, raw, last_seen)
             SELECT m.name, m.model_type, m.source_model, m.target_model,
                    m.source_dim, m.target_dim, m.sequence_len, m.raw::jsonb, now()
               FROM unnest($1::text[], $2::text[], $3::text[], $4::text[],
                           $5::int[], $6::int[], $7::int[], $8::text[])
                    AS m(name, model_type, source_model, target_model,
                         source_dim, target_dim, sequence_len, raw)
             ON CONFLICT (name) DO UPDATE SET
                model_type   = EXCLUDED.model_type,
                source_model = EXCLUDED.source_model,
                target_model = EXCLUDED.target_model,
                source_dim   = EXCLUDED.source_dim,
                target_dim   = EXCLUDED.target_dim,
                sequence_len = EXCLUDED.sequence_len,
                raw          = EXCLUDED.raw,
                last_seen    = now()
             WHERE (postvec.models.model_type, postvec.models.source_model,
                    postvec.models.target_model, postvec.models.source_dim,
                    postvec.models.target_dim, postvec.models.sequence_len,
                    postvec.models.raw)
                   IS DISTINCT FROM
                   (EXCLUDED.model_type, EXCLUDED.source_model,
                    EXCLUDED.target_model, EXCLUDED.source_dim,
                    EXCLUDED.target_dim, EXCLUDED.sequence_len,
                    EXCLUDED.raw)",
            None,
            &[
                names.into(),
                types.into(),
                sources.into(),
                targets.into(),
                source_dims.into(),
                target_dims.into(),
                seq_lens.into(),
                raws.into(),
            ],
        )?;
        Ok(())
    })
}

/// Remove cache rows for models absent from a complete discovery refresh.
/// Never call this after a partial refresh: an unavailable node may be the only
/// node advertising a model.
pub(crate) fn prune_unseen_models(models: &[ModelInfo]) -> Result<(), pgrx::spi::Error> {
    let names: Vec<String> = models.iter().map(|m| m.name.clone()).collect();
    Spi::connect_mut(|client| {
        client.update(
            "DELETE FROM postvec.models WHERE NOT (name = ANY($1::text[]))",
            None,
            &[names.into()],
        )?;
        Ok(())
    })
}

/// Poll the discovery endpoints and refresh the model cache. Returns the
/// number of models seen. In gRPC mode discovery polls the configured
/// the configured HTTP endpoints; in embedded mode it polls the engine host's
/// loopback `/config` listener (`postvec.embedded_http_listen`), so it
/// errors with a transport error while the worker/engine is still coming up.
/// Rarely needed either way — the worker refreshes the cache on the
/// `postvec.model_refresh_interval_ms` cadence.
#[pg_extern]
fn refresh_models() -> i32 {
    let report =
        list_models_report_blocking().unwrap_or_else(|e| error!("postvec: refresh_models: {e}"));
    upsert_models(&report.models).unwrap_or_else(|e| error!("postvec: refresh_models upsert: {e}"));
    if report.complete {
        prune_unseen_models(&report.models)
            .unwrap_or_else(|e| error!("postvec: refresh_models prune: {e}"));
    } else {
        warning!(
            "postvec: refresh_models saw {} models from {}/{} nodes; keeping stale cache rows",
            report.models.len(),
            report.ok_nodes,
            report.ok_nodes + report.failed_nodes
        );
    }
    report.models.len() as i32
}

#[pg_extern]
fn embed(input: &str, model: &str) -> Vec<f32> {
    assert_input_within_cap("embed() input", input.len());
    let texts = vec![input.to_string()];
    let mut vecs = embed_texts(&texts, model, EmbedPurpose::Document)
        .unwrap_or_else(|e| error!("postvec: embed: {e}"));
    if vecs.len() != 1 {
        error!("postvec: embed: expected 1 embedding, got {}", vecs.len());
    }
    vecs.pop().unwrap()
}

#[pg_extern(name = "embed")]
fn embed_set<'a>(
    inputs: pgrx::Array<'a, &'a str>,
    model: &str,
) -> SetOfIterator<'static, Vec<f32>> {
    // Count/byte ceilings run against the BORROWED array before any element
    // is copied into Rust.
    if inputs.len() > 4096 {
        error!(
            "postvec: embed() batch has {} texts; the ceiling is 4096 (split the call)",
            inputs.len()
        );
    }
    let inputs = collect_batch_within_caps("embed() input", inputs);
    let vecs = embed_texts(&inputs, model, EmbedPurpose::Document)
        .unwrap_or_else(|e| error!("postvec: embed: {e}"));
    // Same contract as the scalar overload: the response must be row-parallel
    // to the input, or the caller silently loses/mismatches rows.
    if vecs.len() != inputs.len() {
        error!(
            "postvec: embed: expected {} embeddings, got {}",
            inputs.len(),
            vecs.len()
        );
    }
    SetOfIterator::new(vecs)
}

#[pg_extern]
fn convert(embedding: pgrx::Array<'_, f32>, source_model: &str, target_model: &str) -> Vec<f32> {
    if embedding.is_empty() || embedding.len() > 16_000 {
        // 16,000 is pgvector's dimension ceiling; nothing postvec stores or
        // converts can legitimately be outside it. Checked against the
        // borrowed array datum before any Rust copy.
        error!(
            "postvec: convert: input has {} dims; expected 1..=16000",
            embedding.len()
        );
    }
    let embedding: Vec<f32> = embedding
        .iter()
        .map(|v| {
            v.unwrap_or_else(|| error!("postvec: convert: input embedding contains a NULL element"))
        })
        .collect();
    if !embedding.iter().all(|f| f.is_finite()) {
        error!("postvec: convert: input embedding contains a non-finite component (NaN/Inf)");
    }
    let model =
        resolve_convert(source_model, target_model).unwrap_or_else(|e| error!("postvec: {e}"));
    let vecs = vec![embedding];
    let timeout = query_timeout_ms();
    let client = GrpcClient::from_gucs(timeout);
    let mut out = runtime::block_on_with_timeout(client.overall_timeout_ms(), async {
        client.convert(&vecs, &model).await
    })
    .unwrap_or_else(|e| error!("postvec: convert: {e}"));
    if out.len() != 1 {
        error!("postvec: convert: expected 1 embedding, got {}", out.len());
    }
    out.pop().unwrap()
}

// The one-shot helpers and the cache refresh drive unbounded network work
// (gRPC inference, or a /config fan-out to every node) from the calling
// backend, so PUBLIC does not get them by default. Any DB-authenticated
// role could otherwise use them as a resource amplifier. Grant them back
// per app role as needed:
//   GRANT EXECUTE ON FUNCTION postvec.embed(text, text) TO app_role;
// search()/search_with_vector() stay PUBLIC: they are the query path,
// bounded by postvec.query_timeout_ms.
pgrx::extension_sql!(
    r#"
REVOKE EXECUTE ON FUNCTION postvec.refresh_models() FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION postvec.embed(text, text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION postvec.embed(text[], text) FROM PUBLIC;
REVOKE EXECUTE ON FUNCTION postvec.convert(real[], text, text) FROM PUBLIC;
"#,
    name = "postvec_network_fn_grants",
    requires = [refresh_models, embed, embed_set, convert]
);

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use super::*;

    /// Canned /config payload (the inference host's /config models shape): one embed
    /// model, two chained convert models, a convert-bridge executor, an
    /// embed-bridge executor, a converter to an external (convert-only)
    /// target space, plus a disabled model that must not land in the cache.
    const FIXTURE: &str = r#"{
      "success": true,
      "data": { "models": [
        { "name": "snowflake-arctic-embed-l-v2.0", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "embed", "target_model": "snowflake-arctic-embed-l-v2.0",
            "target_dim": 1024, "sequence_len": 8192 } } },
        { "name": "convert-a-to-b", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert", "source_model": "model-a",
            "target_model": "model-b", "source_dim": 768, "target_dim": 1024 } } },
        { "name": "convert-b-to-c", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert", "source_model": "model-b",
            "target_model": "model-c", "source_dim": 1024, "target_dim": 1536 } } },
        { "name": "convert-snow-to-ext", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert", "source_model": "snowflake-arctic-embed-l-v2.0",
            "target_model": "ext-model", "source_dim": 1024, "target_dim": 1536 } } },
        { "name": "convert-bridge", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert-bridge" } } },
        { "name": "embed-bridge", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "embed-bridge" } } },
        { "name": "ghost", "status": "local",
          "configuration": { "enabled": false, "params": { "model_type": "embed" } } }
      ] }
    }"#;

    fn load_fixture() {
        let models = discovery::parse_config(FIXTURE).expect("fixture parses");
        upsert_models(&models).expect("upsert works");
    }

    #[pg_test]
    fn cache_upsert_from_config_fixture() {
        load_fixture();
        let n = Spi::get_one::<i64>("SELECT count(*) FROM postvec.models").unwrap();
        assert_eq!(n, Some(6)); // disabled model filtered out

        let dim = Spi::get_one::<i32>(
            "SELECT target_dim FROM postvec.models WHERE name = 'snowflake-arctic-embed-l-v2.0'",
        )
        .unwrap();
        assert_eq!(dim, Some(1024));

        // idempotent: second refresh updates, doesn't duplicate
        load_fixture();
        let n2 = Spi::get_one::<i64>("SELECT count(*) FROM postvec.models").unwrap();
        assert_eq!(n2, Some(6));
    }

    #[pg_test]
    fn complete_refresh_prunes_unseen_cache_rows() {
        load_fixture();
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('stale','embed','stale',4,'{}'::jsonb)",
        )
        .unwrap();
        let models = discovery::parse_config(FIXTURE).expect("fixture parses");
        prune_unseen_models(&models).expect("prune works");
        let stale = Spi::get_one::<i64>("SELECT count(*) FROM postvec.models WHERE name = 'stale'")
            .unwrap();
        assert_eq!(stale, Some(0));
    }

    #[pg_test]
    fn embed_resolution() {
        load_fixture();
        assert_eq!(
            resolve_embed("snowflake-arctic-embed-l-v2.0").unwrap(),
            "snowflake-arctic-embed-l-v2.0"
        );
        assert!(matches!(
            resolve_embed("nope"),
            Err(PvError::UnknownModel(_))
        ));
        // convert models are not embed-resolvable
        assert!(resolve_embed("model-b").is_err());
    }

    /// The public `target_model` key must win over an internal-name match —
    /// including when a competing model has target_model NULL (a plain
    /// `ORDER BY bool DESC` is NULLS FIRST and used to rank it above the
    /// exact match).
    #[pg_test]
    fn embed_resolution_prefers_public_target_model_over_name() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('alias', 'embed', NULL,    768, '{}'::jsonb),
                    ('internal-b', 'embed', 'alias', 1024, '{}'::jsonb)",
        )
        .unwrap();
        assert_eq!(
            resolve_embed("alias").unwrap(),
            "internal-b",
            "target_model match outranks a name match with NULL target_model"
        );
        assert_eq!(
            crate::api::registry::resolve_dim("alias"),
            1024,
            "the dimension comes from the target_model-matched row"
        );
    }

    /// Two-tier embed routing: a hosted embed model resolves Direct; a
    /// convert-only model (only reachable as a converter's target) resolves
    /// to an embed-bridge route through the converter's source; a converter
    /// target whose source is itself not embeddable resolves to nothing.
    #[pg_test]
    fn embed_route_resolution_direct_bridge_none() {
        load_fixture();
        assert_eq!(
            resolve_embed_route("snowflake-arctic-embed-l-v2.0").unwrap(),
            EmbedResolution::Direct("snowflake-arctic-embed-l-v2.0".into())
        );
        assert_eq!(
            resolve_embed_route("ext-model").unwrap(),
            EmbedResolution::Bridge {
                executor: "embed-bridge".into(),
                via: "snowflake-arctic-embed-l-v2.0".into(),
                target: "ext-model".into(),
            }
        );
        // model-c is a converter target, but its source (model-b) is not an
        // embed model — no way to produce fresh text embeddings in its space.
        // The detail must name the dead converter, not just say "no path":
        // the operator fix (host the source model) differs from the
        // no-converter case (publish a converter).
        match resolve_embed_route("model-c") {
            Err(PvError::NoEmbedPath { model, detail }) => {
                assert_eq!(model, "model-c");
                assert!(
                    detail.contains("convert-b-to-c") && detail.contains("model-b"),
                    "detail names the converter and its unembeddable source: {detail}"
                );
            }
            other => panic!("expected NoEmbedPath, got {other:?}"),
        }
        // A model no converter targets gets the plainer no-inventory message.
        match resolve_embed_route("nope") {
            Err(PvError::NoEmbedPath { model, detail }) => {
                assert_eq!(model, "nope");
                assert!(
                    detail.contains("no converter targets it"),
                    "detail: {detail}"
                );
            }
            other => panic!("expected NoEmbedPath, got {other:?}"),
        }
    }

    /// A model that is both directly embeddable and a converter target must
    /// embed directly — the bridge is strictly a fallback.
    #[pg_test]
    fn embed_route_prefers_direct_over_bridge() {
        load_fixture();
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('ext-hosted', 'embed', 'ext-model', 1536, '{}'::jsonb)",
        )
        .unwrap();
        assert_eq!(
            resolve_embed_route("ext-model").unwrap(),
            EmbedResolution::Direct("ext-hosted".into())
        );
    }

    /// Direct calls may address an embed model by internal name, but the
    /// engine-side embed-bridge resolver accepts the model's semantic output
    /// name only (`target_model`, falling back to `name` when it is absent).
    /// Do not preflight a route the engine will reject at execution time.
    #[pg_test]
    fn embed_route_bridge_source_matches_engine_resolver_semantics() {
        load_fixture();
        Spi::run(
            "INSERT INTO postvec.models
                    (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('source-internal', 'embed', NULL, 'source-public', 8, '{}'::jsonb),
                    ('convert-internal-ext', 'convert', 'source-internal', 'bad-ext', 9,
                     '{}'::jsonb)",
        )
        .unwrap();

        assert_eq!(
            resolve_embed_route("source-internal").unwrap(),
            EmbedResolution::Direct("source-internal".into()),
            "a direct gRPC call can still use the internal model name"
        );
        match resolve_embed_route("bad-ext") {
            Err(PvError::NoEmbedPath { detail, .. }) => {
                assert!(detail.contains("source-internal"), "detail: {detail}");
            }
            other => panic!("expected NoEmbedPath, got {other:?}"),
        }
    }

    /// The bridge tier needs an embed-bridge executor; without one the
    /// error must say what is missing, not claim the model is unknown.
    #[pg_test]
    fn embed_route_bridge_requires_executor() {
        load_fixture();
        Spi::run("DELETE FROM postvec.models WHERE model_type = 'embed-bridge'").unwrap();
        match resolve_embed_route("ext-model") {
            Err(PvError::NoEmbedPath { model, detail }) => {
                assert_eq!(model, "ext-model");
                assert!(detail.contains("embed-bridge"), "detail: {detail}");
            }
            other => panic!("expected NoEmbedPath, got {other:?}"),
        }
    }

    /// A convert-only model's dimension comes from the converter's
    /// target_dim — no probe, no error.
    #[pg_test]
    fn resolve_dim_from_converter_target_dim() {
        load_fixture();
        assert_eq!(crate::api::registry::resolve_dim("ext-model"), 1536);
    }

    /// enable() must still refuse a model with no embed path at all — a
    /// converter target whose source is not itself embeddable can never get
    /// fresh writes embedded.
    #[pg_test]
    fn enable_refuses_unembeddable_convert_target() {
        load_fixture();
        Spi::run(
            "CREATE TABLE nope_t (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.enable('nope_t','body','model-c', backfill => false)",
            )
            .ok();
        });
        assert!(
            r.is_err(),
            "model-c has a converter but its source is not an embed model"
        );
    }

    /// Only a direct converter resolves. The fixture still holds the
    /// chained pair (a->b, b->c) and a convert-bridge row; a chain that
    /// would have been two-hop is `NoConvertPath` even with every
    /// ingredient present.
    #[pg_test]
    fn convert_resolution_is_direct_only() {
        load_fixture();
        assert_eq!(
            resolve_convert("model-a", "model-b").unwrap(),
            "convert-a-to-b"
        );
        assert!(matches!(
            resolve_convert("model-a", "model-c"),
            Err(PvError::NoConvertPath { .. })
        ));
        assert!(matches!(
            resolve_convert("model-c", "model-a"),
            Err(PvError::NoConvertPath { .. })
        ));
    }

    /// Provider-backed rows in the cache, in the exact shape the gateway's
    /// `descriptor_json` emits: `status: "provider"` plus the top-level
    /// identity fields, which discovery flattens into `raw->'extra'` — the
    /// marker the resolver exclusions key on.
    const PROVIDER_FIXTURE: &str = r#"{
      "success": true,
      "data": { "models": [
        { "name": "snowflake-arctic-embed-l-v2.0", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "embed", "target_model": "snowflake-arctic-embed-l-v2.0",
            "target_dim": 1024, "sequence_len": 8192 } } },
        { "name": "embed-bridge", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "embed-bridge" } } },
        { "name": "convert-bridge", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert-bridge" } } },
        { "name": "convert-b2-to-c2", "status": "local",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert", "source_model": "model-b2",
            "target_model": "model-c2", "source_dim": 8, "target_dim": 8 } } },
        { "name": "univec-convert-snow-to-ext2", "status": "provider",
          "provider": "univec", "provider_file": "univec",
          "provider_model_id": "src-space->tgt-space for snowflake-arctic-embed-l-v2.0[1024]->ext2-model",
          "provider_endpoint": "0123456789abcdef",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert", "source_model": "snowflake-arctic-embed-l-v2.0",
            "target_model": "ext2-model", "source_dim": 1024, "target_dim": 1536 } } },
        { "name": "univec-convert-a2-to-b2", "status": "provider",
          "provider": "univec", "provider_file": "univec",
          "provider_model_id": "a2->b2 for model-a2[8]->model-b2",
          "provider_endpoint": "0123456789abcdef",
          "configuration": { "enabled": true, "params": {
            "model_type": "convert", "source_model": "model-a2",
            "target_model": "model-b2", "source_dim": 8, "target_dim": 8 } } }
      ] }
    }"#;

    fn load_provider_fixture() {
        let models = discovery::parse_config(PROVIDER_FIXTURE).expect("fixture parses");
        upsert_models(&models).expect("upsert works");
    }

    /// A provider-backed converter serves the DIRECT convert route — that is
    /// the feature — but never an embed bridge: a bridge goes on the wire as
    /// one EmbedTexts naming the embed-bridge executor, which resolves its
    /// chain against the ENGINE's resolver, and that resolver cannot see
    /// gateway entries. Before the exclusion this fixture resolved a Bridge
    /// route the host then refused on every call.
    #[pg_test]
    fn provider_converters_resolve_direct_but_never_bridge() {
        load_provider_fixture();
        assert_eq!(
            resolve_convert("snowflake-arctic-embed-l-v2.0", "ext2-model").unwrap(),
            "univec-convert-snow-to-ext2"
        );
        // The marker feeds `migrate()`'s consent NOTICE for hosted
        // conversion; a local converter must not trigger it.
        assert_eq!(
            crate::api::registry::external_provider_of_converter("univec-convert-snow-to-ext2")
                .as_deref(),
            Some("univec")
        );
        assert_eq!(
            crate::api::registry::external_provider_of_converter("convert-b2-to-c2"),
            None
        );
        // ext2-model must have NO embed route, and the refusal names the
        // real situation rather than "nothing targets it".
        match resolve_embed_route("ext2-model").unwrap_err() {
            PvError::NoEmbedPath { detail, .. } => assert!(
                detail.contains("hosted converters serve"),
                "detail: {detail}"
            ),
            other => panic!("expected NoEmbedPath, got {other:?}"),
        }
    }

    /// Two direct converters for one pair, the hosted one sorting FIRST by
    /// name: the local one must still win. Same-prefix names would pass on
    /// the old lexical order and prove nothing.
    #[pg_test]
    fn convert_resolution_prefers_local() {
        load_provider_fixture();
        let models = discovery::parse_config(
            r#"{ "success": true, "data": { "models": [
              { "name": "a-hosted-convert", "status": "provider", "provider": "univec",
                "provider_file": "univec", "provider_model_id": "x", "provider_endpoint": "0",
                "configuration": { "enabled": true, "params": {
                  "model_type": "convert", "source_model": "model-p", "target_model": "model-q",
                  "source_dim": 8, "target_dim": 8 } } },
              { "name": "z-local-convert", "status": "local",
                "configuration": { "enabled": true, "params": {
                  "model_type": "convert", "source_model": "model-p", "target_model": "model-q",
                  "source_dim": 8, "target_dim": 8 } } }
            ] } }"#,
        )
        .expect("fixture parses");
        upsert_models(&models).expect("upsert works");
        assert_eq!(
            resolve_convert("model-p", "model-q").unwrap(),
            "z-local-convert"
        );
        // Positive control: with only hosted routes, lexical order applies.
        assert_eq!(
            resolve_convert("snowflake-arctic-embed-l-v2.0", "ext2-model").unwrap(),
            "univec-convert-snow-to-ext2"
        );
    }

    /// Chained hops never resolve — two-hop conversion went away with the
    /// convert-bridge executor's removal from postvec. This fixture is the
    /// once-worst case (a provider-backed first hop into a local second hop,
    /// plus a convert-bridge executor row still advertised by the cache):
    /// a2→c2 must be NoConvertPath, not a chain.
    #[pg_test]
    fn chained_converters_never_resolve() {
        load_provider_fixture();
        assert!(matches!(
            resolve_convert("model-a2", "model-c2"),
            Err(PvError::NoConvertPath { .. })
        ));
    }

    #[pg_test]
    fn convert_rejects_non_finite_input_before_grpc() {
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<Vec<f32>>(
                "SELECT postvec.convert(ARRAY['NaN'::real], 'model-a', 'model-b')",
            )
            .ok();
        });
        assert!(r.is_err(), "NaN input must error before any gRPC call");

        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<Vec<f32>>(
                "SELECT postvec.convert(ARRAY['Infinity'::real], 'model-a', 'model-b')",
            )
            .ok();
        });
        assert!(r.is_err(), "Infinity input must error before any gRPC call");
    }
}
