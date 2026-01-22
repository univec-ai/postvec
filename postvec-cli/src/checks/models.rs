//! Model-store checks: the CLI-managed half of an engine root.
//!
//! Pure over [`ModelStoreFacts`] / [`RegistryProbeFacts`] — the hashing and
//! the one optional network probe happen in `collect`, gated on `--deep`.
//! `registry.reachable` warns rather than fails when unreachable: an
//! intentionally offline deployment is a supported shape, not an unhealthy
//! one; only `--strict` promotes it.

use super::CheckResult;
use crate::facts::{ModelStoreFacts, RegistryProbeFacts};
use serde_json::json;

pub struct ModelsInput<'a> {
    pub store: Option<&'a ModelStoreFacts>,
    pub registry: Option<&'a RegistryProbeFacts>,
    /// Engine-loaded model names (from the loopback `/config`), when known.
    pub loaded: Option<&'a std::collections::BTreeSet<String>>,
    /// The explicit `postvec.embedded_models` allow-list (empty = scan mode).
    pub allow_list: &'a [String],
    pub deep: bool,
}

pub fn checks(input: &ModelsInput<'_>) -> Vec<CheckResult> {
    let mut out = Vec::new();
    if let Some(store) = input.store {
        out.push(receipts(store));
        out.push(unactivated(store, input.loaded, input.allow_list));
        out.push(enabled_drift(store, input.loaded));
        out.push(integrity(store, input.deep));
        out.push(updatable(store, input.registry, input.deep));
    }
    out.push(registry_reachable(input.registry, input.deep));
    out
}

/// Does the registry offer a newer revision of anything installed? (`--deep`
/// only, for the same reason `registry.reachable` is: an intentionally
/// offline deployment must not be reported unhealthy for not phoning home.)
fn updatable(
    store: &ModelStoreFacts,
    registry: Option<&RegistryProbeFacts>,
    deep: bool,
) -> CheckResult {
    let scope = "embedded";
    if !deep {
        return CheckResult::skip(
            "models.updatable",
            scope,
            "the registry is queried only under --deep",
        );
    }
    let Some(probe) = registry.filter(|p| p.attempted && p.error.is_none()) else {
        return CheckResult::skip(
            "models.updatable",
            scope,
            "the registry channel did not answer, so head revisions are unknown",
        );
    };
    let mut available: Vec<serde_json::Value> = Vec::new();
    let mut not_offered: Vec<String> = Vec::new();
    let mut compared = 0usize;
    for model in store.models.iter().filter(|m| m.cli_owned) {
        // A name the fetched channel does not list cannot be compared: it may
        // be withdrawn, private to another account, or simply absent from the
        // channel this credential sees. Silence would read as "at head".
        let (Some(installed), Some(head)) = (
            model.receipt_revision,
            probe.head_revisions.get(&model.dir_name).copied(),
        ) else {
            not_offered.push(model.dir_name.clone());
            continue;
        };
        compared += 1;
        if head > installed {
            available
                .push(json!({ "name": model.dir_name, "installed": installed, "available": head }));
        }
    }
    if available.is_empty() {
        let channel = probe.channel.as_deref().unwrap_or("selected");
        return CheckResult::pass(
            "models.updatable",
            scope,
            if not_offered.is_empty() {
                format!("all {compared} CLI-installed model(s) are at the registry's head revision")
            } else {
                format!(
                    "{compared} CLI-installed model(s) at the head revision; {} not offered by \
                     the {channel} channel, so not comparable: {}",
                    not_offered.len(),
                    not_offered.join(", ")
                )
            },
        )
        .with_evidence(json!({ "compared": compared, "not_offered": not_offered }));
    }
    let names: Vec<String> = available
        .iter()
        .map(|entry| {
            format!(
                "{} ({}→{})",
                entry["name"].as_str().unwrap_or("?"),
                entry["installed"],
                entry["available"]
            )
        })
        .collect();
    CheckResult::warn(
        "models.updatable",
        scope,
        format!("newer revisions available: {}", names.join(", ")),
    )
    .with_evidence(json!({ "updates": available }))
    .with_fix("run `postvec model upgrade --all` to replace them in place")
}

/// Are CLI receipts well-formed and consistent with descriptor/name/backend?
fn receipts(store: &ModelStoreFacts) -> CheckResult {
    let scope = "embedded";
    let mut problems: Vec<String> = Vec::new();
    let mut cli_count = 0usize;
    for model in &store.models {
        if let Some(error) = &model.receipt_error {
            problems.push(format!("{}: {error}", model.dir_name));
            continue;
        }
        if !model.cli_owned {
            continue;
        }
        cli_count += 1;
        if let Some(receipt_name) = &model.receipt_name {
            if receipt_name != &model.dir_name {
                problems.push(format!(
                    "{}: receipt names {receipt_name:?}",
                    model.dir_name
                ));
            }
        }
        if let Some(receipt_backend) = &model.receipt_backend {
            if receipt_backend != &model.backend {
                problems.push(format!(
                    "{}: receipt says backend {receipt_backend:?}, directory is {:?}",
                    model.dir_name, model.backend
                ));
            }
        }
        if let Some(descriptor_name) = &model.descriptor_name {
            if descriptor_name != &model.dir_name {
                problems.push(format!(
                    "{}: descriptor names itself {descriptor_name:?}; engine lookups by \
                     directory and by name would disagree",
                    model.dir_name
                ));
            }
        }
    }
    if problems.is_empty() {
        CheckResult::pass(
            "models.receipts",
            scope,
            format!(
                "{cli_count} CLI-installed model(s) with consistent receipts \
                 ({} total on disk)",
                store.models.len()
            ),
        )
    } else {
        CheckResult::fail(
            "models.receipts",
            scope,
            format!("{} receipt problem(s)", problems.len()),
        )
        .with_evidence(json!({ "problems": problems }))
        .with_fix(
            "an inconsistent receipt means the directory was hand-edited or half-copied; \
             remove and re-pull the model, or repair the copy from its source",
        )
    }
}

/// Is anything the engine holds marked deactivated on disk?
///
/// The two facts have to agree in both directions. This one is the dangerous
/// direction: the model is serving now but a restart will not bring it back,
/// so a column depending on it fails at the least convenient moment. It means
/// a `deactivate` whose unload did not complete — or, on a shared engine root,
/// another host's flip.
///
/// A **deactivated** model that is simply not loaded is not a finding at all:
/// that is the state `postvec model deactivate` exists to produce.
fn enabled_drift(
    store: &ModelStoreFacts,
    loaded: Option<&std::collections::BTreeSet<String>>,
) -> CheckResult {
    let scope = "embedded";
    let Some(loaded) = loaded else {
        return CheckResult::skip(
            "models.enabled-drift",
            scope,
            "the engine's loaded set is unknown (listener unreachable)",
        );
    };
    let resident_but_disabled: Vec<String> = store
        .models
        .iter()
        .filter(|m| m.cli_owned && !m.enabled)
        .filter(|m| loaded.contains(m.descriptor_name.as_deref().unwrap_or(&m.dir_name)))
        .map(|m| m.dir_name.clone())
        .collect();
    if resident_but_disabled.is_empty() {
        return CheckResult::pass(
            "models.enabled-drift",
            scope,
            "no model is loaded while marked deactivated on disk",
        );
    }
    CheckResult::fail(
        "models.enabled-drift",
        scope,
        format!(
            "loaded but deactivated on disk: {}",
            resident_but_disabled.join(", ")
        ),
    )
    .with_evidence(json!({ "models": resident_but_disabled }))
    .with_fix(format!(
        "these are serving now but will not come back after a restart. Rerun `postvec model \
         deactivate {}` to finish taking them out, or `postvec model activate {}` to keep them",
        resident_but_disabled.join(" "),
        resident_but_disabled.join(" ")
    ))
}

/// Which eligible on-disk models are absent from the engine?
fn unactivated(
    store: &ModelStoreFacts,
    loaded: Option<&std::collections::BTreeSet<String>>,
    allow_list: &[String],
) -> CheckResult {
    let scope = "embedded";
    let Some(loaded) = loaded else {
        return CheckResult::skip(
            "models.unactivated",
            scope,
            "the engine's loaded set is unknown (listener unreachable)",
        );
    };
    let missing: Vec<String> = store
        .models
        .iter()
        .filter(|m| m.cli_owned && m.enabled)
        .filter(|m| allow_list.is_empty() || allow_list.contains(&m.dir_name))
        .filter(|m| {
            let engine_name = m.descriptor_name.as_deref().unwrap_or(&m.dir_name);
            !loaded.contains(engine_name)
        })
        .map(|m| m.dir_name.clone())
        .collect();
    if missing.is_empty() {
        CheckResult::pass(
            "models.unactivated",
            scope,
            "every eligible CLI-installed model is loaded",
        )
    } else {
        CheckResult::warn(
            "models.unactivated",
            scope,
            format!("activated on disk but not loaded: {}", missing.join(", ")),
        )
        // Not "or restart the cluster": a restart reloads exactly what the
        // descriptors say, so if these are enabled and absent, something
        // failed at load time and a restart repeats it.
        .with_fix(format!(
            "run `postvec model activate {}`; if it keeps failing the reason is in the \
             PostgreSQL server log",
            missing.join(" ")
        ))
    }
}

/// Do installed files match receipt sizes and hashes? (`--deep` only.)
fn integrity(store: &ModelStoreFacts, deep: bool) -> CheckResult {
    let scope = "embedded";
    if !deep {
        return CheckResult::skip(
            "models.integrity",
            scope,
            "file hashing runs only under --deep",
        );
    }
    let mut problems: Vec<String> = Vec::new();
    let mut verified = 0usize;
    for model in &store.models {
        match &model.integrity_problems {
            Some(list) if list.is_empty() => verified += 1,
            Some(list) => {
                for problem in list {
                    problems.push(format!("{}: {problem}", model.dir_name));
                }
            }
            None => {}
        }
    }
    if problems.is_empty() {
        CheckResult::pass(
            "models.integrity",
            scope,
            format!("{verified} CLI-installed model(s) match their receipts"),
        )
    } else {
        CheckResult::fail(
            "models.integrity",
            scope,
            format!("{} file(s) differ from their receipts", problems.len()),
        )
        .with_evidence(json!({ "problems": problems }))
        .with_fix(
            "the install was modified after extraction; remove and re-pull the model (a re-pull \
             of the installed revision is byte-identical). If the only differing file is \
             ninference.hub.json, an activate/deactivate was interrupted between rewriting the \
             descriptor and updating the receipt — rerun `postvec model activate <name>` (or \
             `deactivate`) and the receipt is reconciled",
        )
    }
}

/// Does the selected channel index validate, and which credential was used?
fn registry_reachable(registry: Option<&RegistryProbeFacts>, deep: bool) -> CheckResult {
    let scope = "registry";
    if !deep {
        return CheckResult::skip(
            "registry.reachable",
            scope,
            "network probe runs only under --deep",
        );
    }
    let Some(probe) = registry.filter(|p| p.attempted) else {
        return CheckResult::skip("registry.reachable", scope, "no probe was performed");
    };
    let credential = probe
        .credential_source
        .as_deref()
        .unwrap_or("anonymous (public channel)");
    match &probe.error {
        None => CheckResult::pass(
            "registry.reachable",
            scope,
            format!(
                "{} channel: {} models ({credential})",
                probe.channel.as_deref().unwrap_or("?"),
                probe.model_count.unwrap_or(0)
            ),
        ),
        // WARN, not FAIL: air-gapped deployments are a supported shape.
        Some(error) => CheckResult::warn(
            "registry.reachable",
            scope,
            format!("registry index not reachable ({credential}): {error}"),
        )
        .with_fix(
            "harmless if this host is deliberately offline; otherwise check network access \
             and the credential (`postvec whoami`)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckStatus;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn stored(dir: &str, cli_owned: bool) -> crate::facts::StoredModelFacts {
        crate::facts::StoredModelFacts {
            dir_name: dir.into(),
            descriptor_name: Some(dir.into()),
            backend: "onnx-runtime".into(),
            enabled: true,
            cli_owned,
            receipt_error: None,
            receipt_name: cli_owned.then(|| dir.to_string()),
            receipt_backend: cli_owned.then(|| "onnx-runtime".to_string()),
            receipt_revision: cli_owned.then_some(1),
            integrity_problems: None,
        }
    }

    fn store(models: Vec<crate::facts::StoredModelFacts>) -> crate::facts::ModelStoreFacts {
        crate::facts::ModelStoreFacts {
            root: PathBuf::from("/opt/postvec/ninference"),
            models,
        }
    }

    #[test]
    fn ids_are_registered_in_check_order() {
        let input = ModelsInput {
            store: Some(&store(vec![stored("a", true)])),
            registry: None,
            loaded: None,
            allow_list: &[],
            deep: false,
        };
        for check in checks(&input) {
            assert!(
                crate::checks::CHECK_ORDER.contains(&check.id),
                "{} is not in CHECK_ORDER",
                check.id
            );
        }
    }

    #[test]
    fn consistent_receipts_pass_and_a_malformed_one_fails() {
        assert_eq!(
            receipts(&store(vec![stored("a", true)])).status,
            CheckStatus::Pass
        );

        let mut bad = stored("b", false);
        bad.receipt_error = Some("schema_version 99".into());
        let result = receipts(&store(vec![bad]));
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn a_receipt_name_drift_fails() {
        let mut drifted = stored("a", true);
        drifted.receipt_name = Some("other".into());
        assert_eq!(receipts(&store(vec![drifted])).status, CheckStatus::Fail);
    }

    #[test]
    fn unactivated_warns_only_for_eligible_cli_models() {
        let loaded: BTreeSet<String> = BTreeSet::new();
        // Package/manual content is not the CLI's to activate.
        let result = unactivated(&store(vec![stored("pkg", false)]), Some(&loaded), &[]);
        assert_eq!(result.status, CheckStatus::Pass);

        let result = unactivated(&store(vec![stored("mine", true)]), Some(&loaded), &[]);
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.summary.contains("mine"));

        // Under an explicit allow-list, an unlisted model is not "unactivated".
        let result = unactivated(
            &store(vec![stored("mine", true)]),
            Some(&loaded),
            &["other".to_string()],
        );
        assert_eq!(result.status, CheckStatus::Pass);

        // Unknown loaded set → SKIP, not a false warning.
        let result = unactivated(&store(vec![stored("mine", true)]), None, &[]);
        assert_eq!(result.status, CheckStatus::Skip);
    }

    /// The dangerous direction of the disk/engine disagreement: serving now,
    /// gone after a restart. The ordinary "deactivated and not loaded" state
    /// is not a finding — it is exactly what `deactivate` produces.
    #[test]
    fn a_loaded_but_deactivated_model_fails_while_a_quiet_one_passes() {
        let mut off = stored("off", true);
        off.enabled = false;

        let nothing_loaded: BTreeSet<String> = BTreeSet::new();
        let quiet = enabled_drift(&store(vec![off.clone()]), Some(&nothing_loaded));
        assert_eq!(quiet.status, CheckStatus::Pass);

        let still_resident: BTreeSet<String> = ["off".to_string()].into();
        let drifted = enabled_drift(&store(vec![off]), Some(&still_resident));
        assert_eq!(drifted.status, CheckStatus::Fail);
        assert!(drifted.summary.contains("off"), "{}", drifted.summary);
        let fix = drifted.remediation.unwrap();
        assert!(fix.contains("postvec model deactivate off"), "{fix}");
        assert!(fix.contains("postvec model activate off"), "{fix}");

        // An unknown loaded set is a skip, not a false pass.
        assert_eq!(
            enabled_drift(&store(vec![stored("a", true)]), None).status,
            CheckStatus::Skip
        );
    }

    /// A freshly pulled (therefore deactivated) model must not read as
    /// "forgot to activate": pull deliberately stops short of serving.
    #[test]
    fn a_deactivated_model_is_not_reported_as_unactivated() {
        let mut fresh = stored("fresh", true);
        fresh.enabled = false;
        let nothing_loaded: BTreeSet<String> = BTreeSet::new();
        let result = unactivated(&store(vec![fresh]), Some(&nothing_loaded), &[]);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    /// An activated model that did not load is a warning whose fix is to
    /// activate again — never "restart", which reloads exactly the same
    /// descriptors and would repeat the failure.
    #[test]
    fn the_unactivated_fix_names_activate_not_a_restart() {
        let nothing_loaded: BTreeSet<String> = BTreeSet::new();
        let result = unactivated(
            &store(vec![stored("mine", true)]),
            Some(&nothing_loaded),
            &[],
        );
        assert_eq!(result.status, CheckStatus::Warn);
        let fix = result.remediation.unwrap();
        assert!(fix.contains("postvec model activate mine"), "{fix}");
        assert!(!fix.to_lowercase().contains("restart the cluster"), "{fix}");
    }

    #[test]
    fn integrity_skips_shallow_and_fails_on_problems() {
        let clean = store(vec![stored("a", true)]);
        assert_eq!(integrity(&clean, false).status, CheckStatus::Skip);

        let mut verified = stored("a", true);
        verified.integrity_problems = Some(vec![]);
        assert_eq!(
            integrity(&store(vec![verified]), true).status,
            CheckStatus::Pass
        );

        let mut tampered = stored("a", true);
        tampered.integrity_problems = Some(vec!["weights.onnx: sha256 differs".into()]);
        let result = integrity(&store(vec![tampered]), true);
        assert_eq!(result.status, CheckStatus::Fail);
    }

    #[test]
    fn registry_probe_is_skip_shallow_warn_offline_pass_online() {
        assert_eq!(registry_reachable(None, false).status, CheckStatus::Skip);
        assert_eq!(registry_reachable(None, true).status, CheckStatus::Skip);

        let offline = crate::facts::RegistryProbeFacts {
            attempted: true,
            channel: None,
            model_count: None,
            credential_source: None,
            error: Some("connection refused".into()),
            head_revisions: Default::default(),
        };
        let result = registry_reachable(Some(&offline), true);
        assert_eq!(result.status, CheckStatus::Warn);
        assert!(result.remediation.unwrap().contains("deliberately offline"));

        let online = crate::facts::RegistryProbeFacts {
            attempted: true,
            channel: Some("public".into()),
            model_count: Some(15),
            credential_source: None,
            error: None,
            head_revisions: Default::default(),
        };
        assert_eq!(
            registry_reachable(Some(&online), true).status,
            CheckStatus::Pass
        );
    }
}
