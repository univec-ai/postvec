//! The registry index, as the CLI consumes it.
//!
//! The schema itself (types, constants, name rules and the full
//! validation rule set) lives in the shared [`registry_schema`] crate,
//! which the publisher and the aphex gateway also validate through, so
//! the three cannot drift. This module re-exports it, wraps validation
//! errors into [`CliError`] with the CLI's remediation hints and owns
//! the one piece that is client policy rather than schema: closure
//! expansion for `pull`.

use crate::error::{Error as CliError, Result};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub use registry_schema::{
    parse_sha256_digest, valid_model_name, ArchiveInfo, Index, IndexModel, SchemaError, Viewer,
    ViewerEntitlement, KNOWN_ENTITLEMENTS, KNOWN_MODEL_TYPES, MAX_ARCHIVE_BYTES, MAX_INDEX_BYTES,
    MAX_LIST_ITEMS, MAX_MODELS, MAX_NAME_BYTES, MAX_SOURCES, SCHEMA_VERSION,
};

/// Parse and fully validate an index body, mapping schema errors onto the
/// CLI's error type. A schema-version mismatch gets the one actionable fix.
pub fn parse_and_validate(body: &[u8]) -> Result<Index> {
    registry_schema::parse_and_validate(body).map_err(|e| match e {
        SchemaError::SchemaVersion(_) => {
            CliError::precondition(e.to_string()).with_fix("upgrade postvec-cli")
        }
        other => CliError::precondition(other.to_string()),
    })
}

/// The full rule set over an already-deserialized index.
pub fn validate(index: &Index) -> Result<()> {
    registry_schema::validate(index).map_err(|e| CliError::precondition(e.to_string()))
}

/// Why an entry is part of a pull closure, shown in the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PullReason {
    Requested,
    EngineDependency,
    PostvecCompanion,
}

impl PullReason {
    pub fn describe(self) -> &'static str {
        match self {
            PullReason::Requested => "requested",
            PullReason::EngineDependency => "engine dependency",
            PullReason::PostvecCompanion => "postvec companion",
        }
    }
}

/// One entry of an expanded pull closure, in install order (dependencies
/// before their dependents).
#[derive(Debug, Clone)]
pub struct ClosureEntry<'a> {
    pub model: &'a IndexModel,
    pub reason: PullReason,
}

/// Expand `requested` through `dependencies` and `postvec_requires`
/// transitively, deduplicated, in topological (dependency-first) order.
///
/// The index has already been validated, so membership and acyclicity hold;
/// this only refuses names the index does not offer (or has withdrawn).
pub fn expand_closure<'a>(index: &'a Index, requested: &[String]) -> Result<Vec<ClosureEntry<'a>>> {
    let mut reasons: BTreeMap<&str, PullReason> = BTreeMap::new();
    let mut order: Vec<&'a IndexModel> = Vec::new();
    let mut in_progress: BTreeSet<&str> = BTreeSet::new();

    fn visit<'a>(
        index: &'a Index,
        name: &str,
        reason: PullReason,
        reasons: &mut BTreeMap<&'a str, PullReason>,
        order: &mut Vec<&'a IndexModel>,
        in_progress: &mut BTreeSet<&'a str>,
        requested_by: Option<&str>,
    ) -> Result<()> {
        let Some(model) = index.model(name) else {
            let context = match requested_by {
                Some(parent) => format!(" (required by {parent})"),
                None => String::new(),
            };
            // The authenticated view IS this caller's complete catalogue,
            // so one non-leaking message covers every absence — demoted,
            // entitlement-filtered, or never published. It must not imply
            // the name exists in another/paid/hidden catalogue, or the
            // subscriber catalogue leaks one probe at a time.
            if index.authenticated {
                return Err(CliError::precondition(format!(
                    "{name} is not in your catalogue{context}"
                ))
                .with_fix(format!(
                    "check `postvec whoami`; if you expect access, see {}",
                    super::urls::DASHBOARD_URL
                )));
            }
            return Err(CliError::precondition(format!(
                "the selected registry channel does not offer {name:?}{context}"
            ))
            .with_fix(
                "run `postvec model ls --available` to see the catalogue; \
                 private entries need `postvec login`",
            ));
        };
        if model.withdrawn && requested_by.is_none() {
            return Err(CliError::precondition(format!(
                "{name} has been withdrawn from the registry"
            ))
            .with_fix("its successor is listed by `postvec model ls --available`"));
        }
        if let Some(existing) = reasons.get_mut(model.name.as_str()) {
            // Requested beats derived; a stronger reason never downgrades.
            if reason == PullReason::Requested {
                *existing = reason;
            }
            return Ok(());
        }
        if !in_progress.insert(model.name.as_str()) {
            // Unreachable after validate(), but a second line of defence
            // against a cycle is cheaper than a stack overflow.
            return Err(CliError::precondition(format!(
                "dependency cycle through {name}"
            )));
        }
        for dep in &model.dependencies {
            visit(
                index,
                dep,
                PullReason::EngineDependency,
                reasons,
                order,
                in_progress,
                Some(&model.name),
            )?;
        }
        for companion in &model.postvec_requires {
            visit(
                index,
                companion,
                PullReason::PostvecCompanion,
                reasons,
                order,
                in_progress,
                Some(&model.name),
            )?;
        }
        in_progress.remove(model.name.as_str());
        reasons.insert(model.name.as_str(), reason);
        order.push(model);
        Ok(())
    }

    for name in requested {
        visit(
            index,
            name,
            PullReason::Requested,
            &mut reasons,
            &mut order,
            &mut in_progress,
            None,
        )?;
    }

    Ok(order
        .into_iter()
        .map(|model| ClosureEntry {
            model,
            reason: reasons[model.name.as_str()],
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, deps: &[&str], companions: &[&str]) -> IndexModel {
        IndexModel {
            name: name.to_string(),
            access: "public".to_string(),
            model_type: "embed".to_string(),
            backend: "onnx-runtime".to_string(),
            quantization: None,
            source_model: None,
            target_model: None,
            source_dim: None,
            target_dim: Some(384),
            sequence_len: None,
            license: Some("apache-2.0".to_string()),
            license_version: None,
            license_url: None,
            license_acceptance: None,
            source: None,
            dependencies: deps.iter().map(|s| s.to_string()).collect(),
            postvec_requires: companions.iter().map(|s| s.to_string()).collect(),
            min_postvec_version: None,
            published_at: None,
            summary: None,
            eval: None,
            withdrawn: false,
            revision: None,
            required_entitlements: vec![],
            archive: ArchiveInfo {
                digest: format!("sha256:{}", "ab".repeat(32)),
                size: 100,
                installed_size: 90,
                sources: vec!["https://example.invalid/a".to_string()],
            },
        }
    }

    fn index(models: Vec<IndexModel>) -> Index {
        Index {
            schema_version: SCHEMA_VERSION,
            channel: "public".to_string(),
            authenticated: false,
            generated_at: None,
            models,
            viewer: None,
        }
    }

    /// The CLI wrapper's one addition to the shared schema: the
    /// schema-version mismatch carries the "upgrade postvec-cli" fix.
    #[test]
    fn a_newer_schema_version_names_the_upgrade_fix() {
        let mut value = serde_json::to_value(index(vec![entry("a", &[], &[])])).unwrap();
        value["schema_version"] = serde_json::json!(2);
        let err = parse_and_validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(err.to_string().contains("schema_version 2"), "{err}");
        assert_eq!(err.remediation(), Some("upgrade postvec-cli"));
    }

    #[test]
    fn closure_expansion_is_dependency_first_and_labels_reasons() {
        let mut converter = entry("convert-a-to-b", &[], &["embed-a", "embed-bridge"]);
        converter.model_type = "convert".to_string();
        let idx = index(vec![
            entry("embed-a", &[], &[]),
            entry("embed-bridge", &[], &[]),
            converter,
        ]);
        let closure = expand_closure(&idx, &["convert-a-to-b".to_string()]).unwrap();
        let names: Vec<&str> = closure.iter().map(|c| c.model.name.as_str()).collect();
        assert_eq!(names, ["embed-a", "embed-bridge", "convert-a-to-b"]);
        assert_eq!(closure[0].reason, PullReason::PostvecCompanion);
        assert_eq!(closure[2].reason, PullReason::Requested);
    }

    #[test]
    fn closure_expansion_refuses_names_the_channel_does_not_offer() {
        let idx = index(vec![entry("a", &[], &[])]);
        let err = expand_closure(&idx, &["missing".to_string()]).unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
        assert!(err.remediation().unwrap().contains("postvec login"));
    }

    /// On the authenticated view, an absent name (demoted,
    /// entitlement-filtered or never published) gets one non-leaking
    /// message. It must not imply the name exists in another, paid or
    /// hidden catalogue, and its fix points at `whoami` and the dashboard,
    /// never at logging in again.
    #[test]
    fn an_authenticated_absence_is_reported_without_leaking() {
        let mut idx = index(vec![entry("a", &[], &[])]);
        idx.channel = "private".to_string();
        idx.authenticated = true;
        let err = expand_closure(&idx, &["filtered-model".to_string()]).unwrap_err();
        assert_eq!(err.to_string(), "filtered-model is not in your catalogue");
        let fix = err.remediation().unwrap();
        assert!(fix.contains("postvec whoami"), "{fix}");
        assert!(fix.contains("https://univec.ai/dashboard"), "{fix}");
        // Non-leaking: no other/paid/hidden-catalogue implication.
        for leak in ["exists", "paid", "hidden", "private", "login"] {
            assert!(!err.to_string().contains(leak), "{leak}: {err}");
            assert!(!fix.contains(leak), "{leak}: {fix}");
        }
    }

    #[test]
    fn a_withdrawn_entry_is_refused_when_requested_directly() {
        let mut gone = entry("old", &[], &[]);
        gone.withdrawn = true;
        let idx = index(vec![gone]);
        let err = expand_closure(&idx, &["old".to_string()]).unwrap_err();
        assert!(err.to_string().contains("withdrawn"), "{err}");
    }

    #[test]
    fn requested_reason_wins_over_derived() {
        let idx = index(vec![entry("dep", &[], &[]), entry("top", &["dep"], &[])]);
        let closure = expand_closure(&idx, &["top".to_string(), "dep".to_string()]).unwrap();
        let dep = closure.iter().find(|c| c.model.name == "dep").unwrap();
        assert_eq!(dep.reason, PullReason::Requested);
    }
}
