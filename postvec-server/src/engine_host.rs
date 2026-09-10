// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Engine bring-up: which models to load, how hard to fail, and warmup.
//!
//! An explicit `--models` list: any failure is fatal.
//! A scan of the root: one bad model is a warning and the rest load.
//! An ambiguous name is excluded; directory order is not a tie-break.
//! Zero models is a warning; `/ready` stays 503 until one is loaded.

use crate::config::Settings;
use crate::metrics::Metrics;
use crate::models::{self, Inventory};
use engine::{EngineConfig, InferenceEngine, InputData};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Executor key whose warmup payload is known and safe to synthesise.
/// Converters need a vector of the right dimension and bridges need a
/// resolved chain first.
const WARMUP_EXECUTOR: &str = "transformer-sequence-embedding";
/// Warmup budget. A cold ONNX session on a slow disk can take a while, and
/// this is off any caller's clock.
const WARMUP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Default)]
pub struct LoadReport {
    pub requested: Vec<String>,
    pub loaded: Vec<String>,
    /// `(model, reason)` for every model that was expected to load and did
    /// not. Non-empty only in scan mode; an explicit list fails the boot.
    pub failures: Vec<(String, String)>,
}

/// Read the root, decide the load set, and load it.
pub async fn build(
    settings: &Settings,
) -> Result<(Arc<InferenceEngine>, Inventory, LoadReport), String> {
    let inventory = models::inventory(&settings.root)?;
    for warning in &inventory.warnings {
        log::warn!("{warning}");
    }

    let explicit = !settings.models.is_empty();
    let roots = if explicit {
        settings.models.clone()
    } else {
        inventory.loadable()
    };

    // Prove the resident cost of the whole set before creating one native
    // session. Loading an arbitrary prefix and hitting the ceiling halfway
    // would leave the node in a state no configuration describes.
    let index = models::descriptor_index(&settings.root)?;
    let closure = models::closure_of(&index, &roots, settings.max_resident_models)?;
    log::info!(
        "{} model root(s) resolve to {} resident model(s) (ceiling {})",
        roots.len(),
        closure.planned.len(),
        settings.max_resident_models
    );

    let config = Arc::new(EngineConfig {
        root_path: settings.root.clone(),
        // Throughput-oriented engine defaults: onnxruntime's own session
        // threading. `--max-inflight` is the bound, applied at the transport.
        host_policy: Default::default(),
    });
    let engine = Arc::new(InferenceEngine::new(config));

    let mut report = LoadReport {
        requested: roots.clone(),
        ..Default::default()
    };
    for name in &roots {
        match engine.load_model(name).await {
            Ok(()) => {
                log::info!("loaded model {name:?}");
                report.loaded.push(name.clone());
            }
            Err(e) if explicit => {
                return Err(format!(
                    "cannot load model {name:?}, which --models names explicitly: {e}\n\
                     (drop it from the list, or fix the model under {}/{}/)",
                    settings.root.display(),
                    models::MODELS_DIR
                ))
            }
            Err(e) => {
                log::warn!("model {name:?} did not load: {e}");
                report.failures.push((name.clone(), e.to_string()));
            }
        }
    }

    let active = engine.get_active_models();
    if active.is_empty() {
        log::warn!(
            "no models are loaded; /ready will answer 503 until one is. Pull a model \
             (`postvec model pull …`) and load it (`postvec-server load …`), or restart with \
             assets under {}/{}/",
            settings.root.display(),
            models::MODELS_DIR
        );
    } else {
        log::info!("active models: {}", active.join(", "));
    }

    Ok((engine, inventory, report))
}

/// One throwaway prediction per embedding model. `/ready` is true before
/// the first inference has run; without this the first caller pays session
/// warmup. Failures are counted and logged.
pub async fn warm_up(engine: &Arc<InferenceEngine>, metrics: &Metrics) {
    let candidates: Vec<String> = engine
        .get_active_models()
        .into_iter()
        .filter(|name| {
            engine
                .get_model(name)
                .map(|model| model.configuration().executor.key == WARMUP_EXECUTOR)
                .unwrap_or(false)
        })
        .collect();
    if candidates.is_empty() {
        return;
    }

    log::info!("warming up {} model(s)", candidates.len());
    for name in candidates {
        let started = Instant::now();
        let deadline = started + WARMUP_TIMEOUT;
        let result = engine
            .clone()
            .predict_raw_at(
                &name,
                InputData::Json(json!({ "texts": ["postvec-server warmup"] })),
                Default::default(),
                deadline,
            )
            .await;
        match result {
            Ok(_) => log::info!("warmed up {name:?} in {:?}", started.elapsed()),
            Err(e) => {
                metrics.warmup_failed();
                log::warn!(
                    "warmup for {name:?} failed: {e} (the model stays loaded; the first real \
                     request will retry)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ServeArgs;
    use crate::config::{resolve, FileConfig};
    use std::collections::BTreeMap;
    use std::path::Path;

    fn settings_for(root: &Path, flags: ServeArgs) -> Settings {
        resolve(
            &flags,
            &FileConfig::default(),
            &BTreeMap::new(),
            root.to_path_buf(),
            None,
        )
        .unwrap()
    }

    /// `InferenceEngine` has no `Debug`, so the tuple `build` returns cannot
    /// go through `unwrap`/`unwrap_err`. Project it first.
    async fn build_err(settings: &Settings) -> String {
        match build(settings).await {
            Ok(_) => panic!("expected build to fail"),
            Err(e) => e,
        }
    }

    async fn build_ok(settings: &Settings) -> (Arc<InferenceEngine>, Inventory, LoadReport) {
        match build(settings).await {
            Ok(v) => v,
            Err(e) => panic!("expected build to succeed, got: {e}"),
        }
    }

    fn write_model(root: &Path, backend: &str, name: &str, enabled: bool, deps: &[&str]) {
        let dir = root.join(models::MODELS_DIR).join(backend).join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(models::DESCRIPTOR_FILENAME),
            json!({"name": name, "enabled": enabled, "dependencies": deps}).to_string(),
        )
        .unwrap();
    }

    /// The preflight runs before any native session exists, so these cases
    /// are reachable without ONNX Runtime on the test machine.

    #[tokio::test]
    async fn an_empty_root_is_reported_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(models::MODELS_DIR)).unwrap();
        let settings = settings_for(dir.path(), ServeArgs::default());
        let (engine, inventory, report) = build_ok(&settings).await;
        assert!(engine.get_active_models().is_empty());
        assert!(inventory.models.is_empty());
        assert!(report.requested.is_empty());
        assert!(report.failures.is_empty());
    }

    #[tokio::test]
    async fn a_missing_models_directory_fails_with_a_pointer_to_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings_for(dir.path(), ServeArgs::default());
        let err = build_err(&settings).await;
        assert!(err.contains("POSTVEC_SERVER_ROOT"), "{err}");
    }

    #[tokio::test]
    async fn an_explicit_model_that_is_not_on_disk_fails_the_boot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(models::MODELS_DIR)).unwrap();
        let settings = settings_for(
            dir.path(),
            ServeArgs {
                models: Some(vec!["absent".into()]),
                ..Default::default()
            },
        );
        let err = build_err(&settings).await;
        assert!(err.contains("absent"), "{err}");
    }

    #[tokio::test]
    async fn an_explicit_model_that_is_deactivated_fails_the_boot() {
        let dir = tempfile::tempdir().unwrap();
        write_model(dir.path(), "onnx-runtime", "off", false, &[]);
        let settings = settings_for(
            dir.path(),
            ServeArgs {
                models: Some(vec!["off".into()]),
                ..Default::default()
            },
        );
        let err = build_err(&settings).await;
        assert!(err.contains("postvec model activate off"), "{err}");
    }

    /// Scan mode must not load an arbitrary prefix of an oversized root.
    #[tokio::test]
    async fn a_root_over_the_resident_ceiling_refuses_to_start() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..4 {
            write_model(dir.path(), "onnx-runtime", &format!("m{i}"), true, &[]);
        }
        let settings = settings_for(
            dir.path(),
            ServeArgs {
                max_resident_models: Some(3),
                ..Default::default()
            },
        );
        let err = build_err(&settings).await;
        assert!(err.contains("resident ceiling"), "{err}");
    }

    /// Scan mode skips disabled and ambiguous models; the preflight sees
    /// neither, so neither consumes a slot.
    #[tokio::test]
    async fn scan_mode_skips_disabled_and_ambiguous_models() {
        let dir = tempfile::tempdir().unwrap();
        write_model(dir.path(), "onnx-runtime", "off", false, &[]);
        write_model(dir.path(), "onnx-runtime", "dup", true, &[]);
        write_model(dir.path(), "candle", "dup", true, &[]);
        let settings = settings_for(dir.path(), ServeArgs::default());
        let (_, inventory, report) = build_ok(&settings).await;
        assert!(report.requested.is_empty(), "{:?}", report.requested);
        assert_eq!(inventory.enabled(), ["dup", "dup"]);
    }
}
