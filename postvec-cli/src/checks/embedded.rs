//! Checks over an embedded-mode installation.
//!
//! The value here is the three-way reconciliation the extension cannot do for
//! itself: **descriptors on disk** vs **models the engine actually loaded** vs
//! **the `postvec.models` cache**. Those three drift independently, and today
//! diagnosing that means reading the server log.
//!
//! The launcher hosts the engine and has no database connection, so its
//! initialization errors only exist in the PostgreSQL log. This module never
//! scrapes logs; it infers what it can from direct checks and, when those pass
//! while the listener is down, prints the exact log command to run.

use super::CheckResult;
#[cfg(test)]
use super::CheckStatus;
use crate::facts::{BuildInfo, DatabaseCache, EmbeddedProbe, InventoryDiff, RootSource};
use serde_json::json;
use std::time::Duration;

const SCOPE: &str = "embedded";

pub struct EmbeddedInput<'a> {
    pub probe: &'a EmbeddedProbe,
    /// `postvec.build_info()`, if the extension reports it.
    pub build_info: Option<&'a BuildInfo>,
    /// `postvec.embedded_models`, empty for scan-load.
    pub requested_models: &'a [String],
    /// The model cache of each inspected database, kept separate: a union
    /// would let one database's contents cover for another's gaps.
    pub cached_models: &'a [DatabaseCache],
    /// How long the cluster has been up, when known. A stale cache during
    /// startup is expected, not broken.
    pub uptime: Option<Duration>,
    /// `postvec.model_refresh_interval_ms`.
    pub refresh_interval: Duration,
    /// Printed as remediation when the engine is down for no visible reason.
    pub log_command: Option<String>,
    /// Versioned ONNX Runtime libraries found under `libs/` that the loader's
    /// exact-name match will not accept.
    pub versioned_ort_libraries: Vec<std::path::PathBuf>,
}

pub fn checks(input: &EmbeddedInput<'_>) -> Vec<CheckResult> {
    let mut out = vec![build_capability(input), path(input)];
    if input.probe.root.is_none() || input.probe.root_error.is_some() {
        // Without a readable root nothing below is observable, and reporting a
        // cascade of failures would bury the one that matters.
        return out;
    }
    out.push(onnx_runtime(input));
    out.push(descriptors(input));
    out.extend(requested_models(input));
    out.extend(listeners(input));
    out.extend(loaded_models(input));
    out.extend(cache_consistency(input));
    out
}

fn build_capability(input: &EmbeddedInput<'_>) -> CheckResult {
    match input.build_info {
        Some(info) if info.embedded => CheckResult::pass(
            "embedded.build-capability",
            SCOPE,
            "the installed library was built with the 'embedded' feature",
        ),
        Some(_) => CheckResult::fail(
            "embedded.build-capability",
            SCOPE,
            "postvec.mode is 'embedded' but the installed library has no embedded engine",
        )
        .required()
        .with_fix(
            "the worker parks with a warning in this state; install a package built with \
             --features embedded, or switch back to postvec.mode = 'grpc'",
        ),
        None => CheckResult::skip(
            "embedded.build-capability",
            SCOPE,
            "this postvec version does not report build features, so embedded support cannot \
             be proven",
        )
        .required()
        .with_fix("upgrade postvec to a version providing postvec.build_info()"),
    }
}

fn path(input: &EmbeddedInput<'_>) -> CheckResult {
    if let Some(error) = &input.probe.root_error {
        return CheckResult::fail("embedded.path", SCOPE, error.clone())
            .required()
            .with_fix(
                "set postvec.path to an absolute engine root containing libs/ and \
                 models/, and restart (it is a POSTMASTER setting)",
            );
    }
    let root = input.probe.root.as_ref().expect("checked above");
    let source = match input.probe.root_source {
        RootSource::Guc => "from postvec.path",
        RootSource::Override => "from --path (diagnostic override only)",
        RootSource::Unknown => "source unknown",
    };
    if input.probe.readable_by_server == Some(false) {
        return CheckResult::fail(
            "embedded.path",
            SCOPE,
            format!(
                "the PostgreSQL account cannot read {} ({source})",
                root.display()
            ),
        )
        .required()
        .with_fix("grant the PostgreSQL account read and traverse permission on the engine root");
    }
    if let Some(error) = &input.probe.models_dir_error {
        return CheckResult::fail(
            "embedded.path",
            SCOPE,
            format!("{}/models is unusable: {error}", root.display()),
        )
        .required()
        .with_fix("the engine loads models from <root>/models/<backend>/<model>/");
    }
    let mut check = CheckResult::pass(
        "embedded.path",
        SCOPE,
        format!("engine root {} ({source})", root.display()),
    );
    if input.probe.readable_by_server.is_none() {
        check = check.with_evidence(json!({
            "note": "readability by the PostgreSQL account could not be verified"
        }));
    }
    check
}

fn onnx_runtime(input: &EmbeddedInput<'_>) -> CheckResult {
    if let Some(found) = input.probe.ort_libraries.first() {
        return CheckResult::pass(
            "embedded.onnx-runtime",
            SCOPE,
            format!("ONNX Runtime found at {}", found.display()),
        )
        .with_evidence(json!({"libraries": input.probe.ort_libraries}));
    }
    // A versioned library alone is the classic packaging mistake: the loader
    // matches the filename exactly, so `libonnxruntime.so.1.22.0` is invisible
    // to it and the error message does not obviously mean "add a symlink".
    if !input.versioned_ort_libraries.is_empty() {
        return CheckResult::fail(
            "embedded.onnx-runtime",
            SCOPE,
            "only a versioned ONNX Runtime library is present; the engine matches the \
             filename exactly",
        )
        .required()
        .with_evidence(json!({"found": input.versioned_ort_libraries}))
        .with_fix(
            "add an unversioned name alongside it (for example a libonnxruntime.so symlink) \
             under the engine root's libs/ directory",
        );
    }
    CheckResult::fail(
        "embedded.onnx-runtime",
        SCOPE,
        "no ONNX Runtime library under the engine root's libs/ directory",
    )
    .required()
    .with_fix("the engine dlopens it at startup; without it the engine cannot initialize")
}

fn descriptors(input: &EmbeddedInput<'_>) -> CheckResult {
    let probe = input.probe;
    let mut problems = Vec::new();
    if !probe.descriptor_errors.is_empty() {
        problems.push(format!(
            "{} unreadable descriptor(s)",
            probe.descriptor_errors.len()
        ));
    }
    // Two descriptors claiming one name means the engine registers whichever it
    // scanned last, silently.
    let mut seen: std::collections::BTreeMap<&str, usize> = Default::default();
    for descriptor in &probe.descriptors {
        *seen.entry(descriptor.name.as_str()).or_default() += 1;
    }
    let duplicates: Vec<&str> = seen
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| *name)
        .collect();
    if !duplicates.is_empty() {
        problems.push(format!(
            "duplicate model name(s): {}",
            duplicates.join(", ")
        ));
    }

    let mismatched: Vec<String> = probe
        .descriptors
        .iter()
        .filter(|descriptor| descriptor.name_mismatch())
        .map(|descriptor| {
            format!(
                "{} declares itself {}",
                descriptor.dir_name, descriptor.name
            )
        })
        .collect();

    let evidence = json!({
        "descriptors": probe.descriptors.len(),
        "enabled": probe.descriptors.iter().filter(|d| d.enabled).count(),
        "errors": probe.descriptor_errors,
        "directory_name_mismatches": mismatched,
    });

    if !problems.is_empty() {
        return CheckResult::fail("embedded.descriptors", SCOPE, problems.join("; "))
            .with_evidence(evidence)
            .with_fix(
                "each model directory holds one ninference.hub.json with a unique \"name\"; the \
             engine skips a descriptor it cannot parse and logs it",
            );
    }
    if probe.descriptors.is_empty() {
        return CheckResult::warn(
            "embedded.descriptors",
            SCOPE,
            "no model descriptors under the engine root",
        )
        .with_evidence(evidence)
        .with_fix(
            "an empty engine is valid for plumbing tests, but nothing can be embedded until a \
             model exists at models/<backend>/<model>/ninference.hub.json",
        );
    }
    if !mismatched.is_empty() {
        return CheckResult::warn(
            "embedded.descriptors",
            SCOPE,
            format!(
                "{} descriptor(s) declare a name different from their directory",
                mismatched.len()
            ),
        )
        .with_evidence(evidence)
        .with_fix(
            "both engine lookups then disagree: postvec.embedded_models matches the directory \
             name, while the loaded model is registered under the declared name",
        );
    }
    CheckResult::pass(
        "embedded.descriptors",
        SCOPE,
        format!(
            "{} descriptor(s) parse, {} enabled",
            probe.descriptors.len(),
            probe.descriptors.iter().filter(|d| d.enabled).count()
        ),
    )
    .with_evidence(evidence)
}

fn requested_models(input: &EmbeddedInput<'_>) -> Option<CheckResult> {
    if input.requested_models.is_empty() {
        return Some(CheckResult::pass(
            "embedded.requested-models",
            SCOPE,
            format!(
                "postvec.embedded_models is empty, so every enabled descriptor is loaded ({})",
                input.probe.expected_names(&[]).len()
            ),
        ));
    }
    // An explicit entry names a model *directory*: that is how the engine
    // resolves a requested model.
    let directories = input.probe.directory_names();
    let missing: Vec<&String> = input
        .requested_models
        .iter()
        .filter(|name| !directories.contains(*name))
        .collect();
    Some(if missing.is_empty() {
        CheckResult::pass(
            "embedded.requested-models",
            SCOPE,
            format!(
                "all {} requested model(s) exist on disk",
                input.requested_models.len()
            ),
        )
    } else {
        CheckResult::fail(
            "embedded.requested-models",
            SCOPE,
            format!(
                "requested model(s) not found on disk: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .required()
        .with_evidence(json!({"available_directories": directories}))
        .with_fix(
            "postvec.embedded_models names model *directories* under <root>/models/<backend>/; \
             a convert-only target also needs its converter and the embed-bridge executor",
        )
    })
}

fn listeners(input: &EmbeddedInput<'_>) -> Vec<CheckResult> {
    let mut out = Vec::new();
    if let Some(listener) = &input.probe.grpc_listener {
        out.push(if listener.connected {
            CheckResult::pass(
                "embedded.grpc-listener",
                SCOPE,
                format!("the loopback gRPC listener on {} answers", listener.address),
            )
        } else {
            CheckResult::fail(
                "embedded.grpc-listener",
                SCOPE,
                format!(
                    "the loopback gRPC listener on {} does not answer: {}",
                    listener.address,
                    listener.error.as_deref().unwrap_or("no further detail")
                ),
            )
            .required()
            .with_fix(engine_down_hint(input))
        });
    }
    if let Some(listener) = &input.probe.http_listener {
        out.push(match (&input.probe.loaded, &input.probe.loaded_error) {
            (Some(inventory), _) => CheckResult::pass(
                "embedded.http-listener",
                SCOPE,
                format!(
                    "the engine's /config on {} reports {} loaded model(s)",
                    listener.address,
                    inventory.models.len()
                ),
            ),
            (None, error) => CheckResult::fail(
                "embedded.http-listener",
                SCOPE,
                format!(
                    "the engine's /config on {} is not usable: {}",
                    listener.address,
                    error.as_deref().unwrap_or("no further detail")
                ),
            )
            .required()
            .with_fix(engine_down_hint(input)),
        });
    }
    out
}

/// What to do when a listener is down but every direct check passed. The
/// launcher's engine-init failures live only in the server log, so name the
/// command rather than guessing — and never run it.
fn engine_down_hint(input: &EmbeddedInput<'_>) -> String {
    let mut hint = String::from(
        "the launcher hosts the engine and retries initialization every 30 seconds; \
         model loading can take a while after a restart",
    );
    if let Some(command) = &input.log_command {
        hint.push_str(". Engine-init errors appear only in the server log: ");
        hint.push_str(command);
    }
    hint
}

fn loaded_models(input: &EmbeddedInput<'_>) -> Option<CheckResult> {
    let loaded = input.probe.loaded.as_ref()?;
    let expected = input.probe.expected_names(input.requested_models);
    let loaded_names = loaded.all_names();
    let missing: Vec<String> = expected.difference(&loaded_names).cloned().collect();

    if loaded_names.is_empty() && expected.is_empty() {
        return Some(
            CheckResult::warn(
                "embedded.loaded-models",
                SCOPE,
                "the engine is up with zero models loaded",
            )
            .with_fix(
                "nothing can be embedded until a model exists and is enabled; the model set is \
                 fixed at engine start, so adding one needs a restart",
            ),
        );
    }
    Some(if missing.is_empty() {
        CheckResult::pass(
            "embedded.loaded-models",
            SCOPE,
            format!("all {} expected model(s) are loaded", expected.len()),
        )
        .with_evidence(json!({"loaded": loaded_names}))
    } else {
        CheckResult::fail(
            "embedded.loaded-models",
            SCOPE,
            format!(
                "{} expected model(s) are not loaded: {}",
                missing.len(),
                missing.join(", ")
            ),
        )
        .with_evidence(json!({"expected": expected, "loaded": loaded_names}))
        .with_fix(format!(
            "a descriptor's presence never proves it loaded — per-model failures are logged \
             and skipped. {}",
            input
                .log_command
                .clone()
                .unwrap_or_else(|| "check the PostgreSQL server log".to_string())
        ))
    })
}

fn cache_consistency(input: &EmbeddedInput<'_>) -> Option<CheckResult> {
    let loaded = input.probe.loaded.as_ref()?;
    let loaded_names = loaded.all_names();
    let expected = input.probe.expected_names(input.requested_models);

    // Per database, never unioned: two databases each holding half the loaded
    // models would otherwise look consistent together while both are wrong.
    let mut drifted = serde_json::Map::new();
    for cache in input.cached_models {
        let diff = InventoryDiff::compute(&expected, &loaded_names, &cache.models);
        if diff.loaded_not_cached.is_empty() && diff.cached_not_loaded.is_empty() {
            continue;
        }
        drifted.insert(
            cache.database.clone(),
            json!({
                "loaded_not_cached": diff.loaded_not_cached,
                "cached_not_loaded": diff.cached_not_loaded,
            }),
        );
    }
    if drifted.is_empty() {
        return Some(CheckResult::pass(
            "embedded.cache-consistency",
            SCOPE,
            "every inspected database's cache matches the models the engine loaded",
        ));
    }

    let enabled_not_loaded = InventoryDiff::compute(&expected, &loaded_names, &loaded_names);
    let evidence = json!({
        "enabled_on_disk_not_loaded": enabled_not_loaded.expected_not_loaded,
        "databases": drifted,
        "descriptor_errors": input.probe.descriptor_errors,
    });
    // During startup the workers have not refreshed yet; that is a warning with
    // the elapsed time, not a failure.
    let settled = input
        .uptime
        .map(|uptime| uptime > input.refresh_interval * 2)
        .unwrap_or(false);
    Some(if settled {
        CheckResult::fail(
            "embedded.cache-consistency",
            SCOPE,
            format!(
                "{} database(s) have a cache that does not match the loaded models: {}",
                drifted.len(),
                drifted.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        )
        .with_evidence(evidence)
        .with_fix(
            "run SELECT postvec.refresh_models() in the affected database(s); the workers also \
             refresh on postvec.model_refresh_interval_ms",
        )
    } else {
        CheckResult::warn(
            "embedded.cache-consistency",
            SCOPE,
            format!(
                "{} database(s) do not match the loaded models yet{}",
                drifted.len(),
                input
                    .uptime
                    .map(|uptime| format!(
                        " (the cluster has been up {})",
                        humantime::format_duration(Duration::from_secs(uptime.as_secs()))
                    ))
                    .unwrap_or_default()
            ),
        )
        .with_evidence(evidence)
        .with_fix("the workers refresh the cache shortly after the engine comes up")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{
        ConfigInventory, DescriptorError, DiskModel, InventoryModel, ListenerProbe,
    };
    use std::path::PathBuf;

    fn model(name: &str, enabled: bool) -> DiskModel {
        DiskModel {
            name: name.into(),
            dir_name: name.into(),
            enabled,
            backend: Some("onnx-runtime".into()),
            dependencies: vec![],
            path: PathBuf::from(format!(
                "/opt/nin/models/onnx-runtime/{name}/ninference.hub.json"
            )),
        }
    }

    fn listener(address: &str, connected: bool) -> ListenerProbe {
        ListenerProbe {
            address: address.into(),
            connected,
            error: (!connected).then(|| "connection refused".to_string()),
        }
    }

    fn inventory(names: &[&str]) -> ConfigInventory {
        ConfigInventory {
            models: names
                .iter()
                .map(|name| InventoryModel {
                    name: (*name).to_string(),
                    enabled: true,
                    model_type: Some("embed".into()),
                    provider: None,
                    provider_file: None,
                    provider_endpoint: None,
                    provider_model_id: None,
                    target_dim: None,
                    space: None,
                    priority: None,
                })
                .collect(),
        }
    }

    fn healthy_probe() -> EmbeddedProbe {
        let mut probe = EmbeddedProbe::new(Some(PathBuf::from("/opt/nin")), RootSource::Guc);
        probe.models_dir = Some(PathBuf::from("/opt/nin/models"));
        probe.readable_by_server = Some(true);
        probe.ort_libraries = vec![PathBuf::from("/opt/nin/libs/linux/libonnxruntime.so")];
        probe.descriptors = vec![model("baai-bge-m3", true), model("disabled-one", false)];
        probe.grpc_listener = Some(listener("127.0.0.1:33433", true));
        probe.http_listener = Some(listener("127.0.0.1:33434", true));
        probe.loaded = Some(inventory(&["baai-bge-m3"]));
        probe
    }

    fn build_info(embedded: bool) -> BuildInfo {
        BuildInfo {
            version: "0.1.0".into(),
            diagnostics_api: 1,
            embedded,
            model_backends: embedded.then(|| vec!["onnx-runtime".into(), "generic".into()]),
        }
    }

    fn caches(names: &[&str]) -> Vec<DatabaseCache> {
        vec![DatabaseCache {
            database: "univec".into(),
            models: names.iter().map(|n| n.to_string()).collect(),
        }]
    }

    fn run(probe: &EmbeddedProbe, requested: &[String], cached: &[&str]) -> Vec<CheckResult> {
        let cached = caches(cached);
        checks(&EmbeddedInput {
            probe,
            build_info: Some(&build_info(true)),
            requested_models: requested,
            cached_models: &cached,
            uptime: Some(Duration::from_secs(3600)),
            refresh_interval: Duration::from_secs(60),
            log_command: Some(
                "journalctl -u postgresql@18-main.service --since -10m --grep postvec".into(),
            ),
            versioned_ort_libraries: vec![],
        })
    }

    fn status(checks: &[CheckResult], id: &str) -> Option<CheckStatus> {
        checks.iter().find(|c| c.id == id).map(|c| c.status)
    }

    #[test]
    fn a_healthy_embedded_installation_passes() {
        let checks = run(&healthy_probe(), &[], &["baai-bge-m3"]);
        for check in &checks {
            assert_eq!(
                check.status,
                CheckStatus::Pass,
                "{} should pass: {}",
                check.id,
                check.summary
            );
        }
    }

    #[test]
    fn every_emitted_id_is_registered() {
        let mut probe = healthy_probe();
        probe.descriptor_errors = vec![DescriptorError {
            path: PathBuf::from("/opt/nin/models/x/y/ninference.hub.json"),
            error: "missing field name".into(),
        }];
        for check in run(&probe, &["baai-bge-m3".to_string()], &["other"]) {
            assert!(
                super::super::CHECK_ORDER.contains(&check.id),
                "{} is not in CHECK_ORDER",
                check.id
            );
        }
    }

    #[test]
    fn a_thin_build_can_never_look_healthy() {
        let cached: Vec<DatabaseCache> = Vec::new();
        let checks = checks(&EmbeddedInput {
            probe: &healthy_probe(),
            build_info: Some(&build_info(false)),
            requested_models: &[],
            cached_models: &cached,
            uptime: Some(Duration::from_secs(3600)),
            refresh_interval: Duration::from_secs(60),
            log_command: None,
            versioned_ort_libraries: vec![],
        });
        let capability = checks
            .iter()
            .find(|c| c.id == "embedded.build-capability")
            .unwrap();
        assert_eq!(capability.status, CheckStatus::Fail);
        assert!(capability.is_blocking());
        assert!(capability.remediation.clone().unwrap().contains("parks"));
    }

    #[test]
    fn unknown_build_capability_is_a_required_skip() {
        let cached: Vec<DatabaseCache> = Vec::new();
        let checks = checks(&EmbeddedInput {
            probe: &healthy_probe(),
            build_info: None,
            requested_models: &[],
            cached_models: &cached,
            uptime: None,
            refresh_interval: Duration::from_secs(60),
            log_command: None,
            versioned_ort_libraries: vec![],
        });
        let capability = checks
            .iter()
            .find(|c| c.id == "embedded.build-capability")
            .unwrap();
        assert_eq!(capability.status, CheckStatus::Skip);
        assert!(
            capability.is_blocking(),
            "an unprovable capability must not read as healthy"
        );
    }

    #[test]
    fn a_bad_root_stops_the_cascade() {
        let mut probe = EmbeddedProbe::new(None, RootSource::Unknown);
        probe.root_error = Some("no engine root is configured".into());
        let checks = run(&probe, &[], &[]);
        assert_eq!(checks.len(), 2, "capability and path only");
        assert_eq!(status(&checks, "embedded.path"), Some(CheckStatus::Fail));
        assert!(checks
            .iter()
            .find(|c| c.id == "embedded.path")
            .unwrap()
            .is_blocking());
    }

    #[test]
    fn an_unreadable_root_is_reported_from_the_servers_point_of_view() {
        let mut probe = healthy_probe();
        probe.readable_by_server = Some(false);
        let checks = run(&probe, &[], &[]);
        let path = checks.iter().find(|c| c.id == "embedded.path").unwrap();
        assert_eq!(path.status, CheckStatus::Fail);
        assert!(path.summary.contains("PostgreSQL account"));
    }

    #[test]
    fn a_versioned_only_onnx_runtime_is_diagnosed_precisely() {
        let mut probe = healthy_probe();
        probe.ort_libraries = vec![];
        let cached: Vec<DatabaseCache> = Vec::new();
        let checks = checks(&EmbeddedInput {
            probe: &probe,
            build_info: Some(&build_info(true)),
            requested_models: &[],
            cached_models: &cached,
            uptime: None,
            refresh_interval: Duration::from_secs(60),
            log_command: None,
            versioned_ort_libraries: vec![PathBuf::from("/opt/nin/libs/libonnxruntime.so.1.22.0")],
        });
        let ort = checks
            .iter()
            .find(|c| c.id == "embedded.onnx-runtime")
            .unwrap();
        assert_eq!(ort.status, CheckStatus::Fail);
        assert!(ort.remediation.clone().unwrap().contains("symlink"));
    }

    #[test]
    fn a_missing_onnx_runtime_fails() {
        let mut probe = healthy_probe();
        probe.ort_libraries = vec![];
        let ort_status = status(&run(&probe, &[], &[]), "embedded.onnx-runtime");
        assert_eq!(ort_status, Some(CheckStatus::Fail));
    }

    #[test]
    fn broken_descriptors_are_all_reported_in_one_run() {
        let mut probe = healthy_probe();
        probe.descriptor_errors = vec![
            DescriptorError {
                path: PathBuf::from("/opt/nin/models/a/b/ninference.hub.json"),
                error: "expected value at line 1".into(),
            },
            DescriptorError {
                path: PathBuf::from("/opt/nin/models/a/c/ninference.hub.json"),
                error: "field \"name\" is empty".into(),
            },
        ];
        let checks = run(&probe, &[], &["baai-bge-m3"]);
        let descriptors = checks
            .iter()
            .find(|c| c.id == "embedded.descriptors")
            .unwrap();
        assert_eq!(descriptors.status, CheckStatus::Fail);
        assert!(descriptors.summary.contains("2 unreadable"));
    }

    #[test]
    fn duplicate_model_names_fail_because_the_winner_is_arbitrary() {
        let mut probe = healthy_probe();
        probe.descriptors = vec![model("dup", true), {
            let mut other = model("dup", true);
            other.dir_name = "other-dir".into();
            other
        }];
        probe.loaded = Some(inventory(&["dup"]));
        let checks = run(&probe, &[], &["dup"]);
        let descriptors = checks
            .iter()
            .find(|c| c.id == "embedded.descriptors")
            .unwrap();
        assert_eq!(descriptors.status, CheckStatus::Fail);
        assert!(descriptors.summary.contains("duplicate"));
    }

    #[test]
    fn an_empty_engine_root_warns_rather_than_pretending_to_be_healthy() {
        let mut probe = healthy_probe();
        probe.descriptors = vec![];
        probe.loaded = Some(ConfigInventory::default());
        let checks = run(&probe, &[], &[]);
        assert_eq!(
            status(&checks, "embedded.descriptors"),
            Some(CheckStatus::Warn)
        );
        assert_eq!(
            status(&checks, "embedded.loaded-models"),
            Some(CheckStatus::Warn)
        );
        assert!(!checks
            .iter()
            .any(|c| c.status == CheckStatus::Pass && c.id == "embedded.loaded-models"));
    }

    #[test]
    fn a_requested_model_missing_from_disk_fails() {
        let probe = healthy_probe();
        let checks = run(&probe, &["not-there".to_string()], &[]);
        let requested = checks
            .iter()
            .find(|c| c.id == "embedded.requested-models")
            .unwrap();
        assert_eq!(requested.status, CheckStatus::Fail);
        assert!(requested.is_blocking());
        assert!(requested
            .remediation
            .clone()
            .unwrap()
            .contains("directories"));
    }

    #[test]
    fn a_descriptor_present_but_not_loaded_is_diagnosed_as_such() {
        let mut probe = healthy_probe();
        probe.descriptors = vec![model("loaded-one", true), model("skipped-one", true)];
        probe.loaded = Some(inventory(&["loaded-one"]));
        let checks = run(&probe, &[], &["loaded-one"]);
        let loaded = checks
            .iter()
            .find(|c| c.id == "embedded.loaded-models")
            .unwrap();
        assert_eq!(loaded.status, CheckStatus::Fail);
        assert!(loaded.summary.contains("skipped-one"));
        assert!(
            loaded.remediation.clone().unwrap().contains("journalctl"),
            "the log command is printed, never executed"
        );
    }

    #[test]
    fn a_down_listener_points_at_the_log_without_running_it() {
        let mut probe = healthy_probe();
        probe.grpc_listener = Some(listener("127.0.0.1:33433", false));
        probe.http_listener = Some(listener("127.0.0.1:33434", false));
        probe.loaded = None;
        probe.loaded_error = Some("the engine's /config listener is not accepting".into());
        let checks = run(&probe, &[], &[]);
        let grpc = checks
            .iter()
            .find(|c| c.id == "embedded.grpc-listener")
            .unwrap();
        assert_eq!(grpc.status, CheckStatus::Fail);
        assert!(grpc.is_blocking());
        assert!(grpc.remediation.clone().unwrap().contains("journalctl"));
        assert!(grpc.remediation.clone().unwrap().contains("30 seconds"));
        // With no engine inventory there is nothing to reconcile against.
        assert_eq!(status(&checks, "embedded.loaded-models"), None);
        assert_eq!(status(&checks, "embedded.cache-consistency"), None);
    }

    #[test]
    fn cache_drift_is_a_warning_during_startup_and_a_failure_once_settled() {
        let probe = healthy_probe();
        let cached = caches(&["stale"]);

        let starting = checks(&EmbeddedInput {
            probe: &probe,
            build_info: Some(&build_info(true)),
            requested_models: &[],
            cached_models: &cached,
            uptime: Some(Duration::from_secs(10)),
            refresh_interval: Duration::from_secs(60),
            log_command: None,
            versioned_ort_libraries: vec![],
        });
        assert_eq!(
            status(&starting, "embedded.cache-consistency"),
            Some(CheckStatus::Warn)
        );

        let settled = checks(&EmbeddedInput {
            probe: &probe,
            build_info: Some(&build_info(true)),
            requested_models: &[],
            cached_models: &cached,
            uptime: Some(Duration::from_secs(3600)),
            refresh_interval: Duration::from_secs(60),
            log_command: None,
            versioned_ort_libraries: vec![],
        });
        let consistency = settled
            .iter()
            .find(|c| c.id == "embedded.cache-consistency")
            .unwrap();
        assert_eq!(consistency.status, CheckStatus::Fail);
        let evidence = format!("{:?}", consistency.evidence);
        assert!(evidence.contains("loaded_not_cached"));
        assert!(evidence.contains("cached_not_loaded"));
        assert!(evidence.contains("baai-bge-m3"));
        assert!(evidence.contains("stale"));
    }

    /// Same unsoundness as the remote side: split caches must not cover for
    /// each other.
    #[test]
    fn each_databases_cache_is_reconciled_on_its_own() {
        let mut probe = healthy_probe();
        probe.descriptors = vec![model("a", true), model("b", true)];
        probe.loaded = Some(inventory(&["a", "b"]));
        let split = vec![
            DatabaseCache {
                database: "alpha".into(),
                models: ["a".to_string()].into(),
            },
            DatabaseCache {
                database: "beta".into(),
                models: ["b".to_string()].into(),
            },
        ];
        let checks = checks(&EmbeddedInput {
            probe: &probe,
            build_info: Some(&build_info(true)),
            requested_models: &[],
            cached_models: &split,
            uptime: Some(Duration::from_secs(3600)),
            refresh_interval: Duration::from_secs(60),
            log_command: None,
            versioned_ort_libraries: vec![],
        });
        let consistency = checks
            .iter()
            .find(|c| c.id == "embedded.cache-consistency")
            .unwrap();
        assert_eq!(
            consistency.status,
            CheckStatus::Fail,
            "a union of the two caches would have looked complete"
        );
        let evidence = format!("{:?}", consistency.evidence);
        assert!(evidence.contains("alpha"), "{evidence}");
        assert!(evidence.contains("beta"), "{evidence}");
    }

    #[test]
    fn scan_load_expects_exactly_the_enabled_descriptors() {
        let probe = healthy_probe();
        let checks = run(&probe, &[], &["baai-bge-m3"]);
        let requested = checks
            .iter()
            .find(|c| c.id == "embedded.requested-models")
            .unwrap();
        assert_eq!(requested.status, CheckStatus::Pass);
        assert!(requested.summary.contains("empty"));
        // `disabled-one` is on disk but disabled, so not expected to be loaded.
        assert_eq!(
            status(&checks, "embedded.loaded-models"),
            Some(CheckStatus::Pass)
        );
    }
}
