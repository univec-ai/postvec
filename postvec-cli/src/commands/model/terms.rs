//! Versioned terms notice for `model pull` / `model upgrade`.
//!
//! After dependency expansion and preflight, the entries that will actually
//! be downloaded or replaced are grouped by their exact terms document
//! (license id, document version, canonical URL, acceptance policy) into
//! the plan's `terms` block. A `none` policy is displayed. A `notice`
//! policy must be acknowledged once per distinct document before any
//! download and before the ordinary plan confirmation, interactively or
//! with the exact `--accept-license <id>@<version>` flag. `--yes` answers
//! only the ordinary mutation confirmation.
//!
//! The acknowledgement is local evidence that this host was shown the
//! exact document, recorded in the install receipt. An index that requires
//! `organization` acceptance is refused here; the server gate is the
//! authority for that policy.
//!
//! A flag naming a document this run does not need (unknown, wrong
//! version, a `none` policy, or one already evidenced by the installation
//! being replaced) is refused, so a copied command line cannot
//! pre-acknowledge the next document.

use crate::error::{CliError, Result};
use crate::plan::{Prompt, TermsDocument};
use crate::registry::index::IndexModel;
use crate::registry::receipt::{LicenseEvidence, Receipt};
use std::collections::{BTreeMap, BTreeSet};

/// A parsed `--accept-license <id>@<version>` token.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AcceptedDocument {
    pub license: String,
    pub version: String,
}

impl AcceptedDocument {
    fn token(&self) -> String {
        format!("{}@{}", self.license, self.version)
    }
}

/// Parse the repeatable `--accept-license` values, strictly: exactly one
/// `@`, a well-shaped id and version, no duplicates, no normalization.
pub fn parse_accept_license(raw: &[String]) -> Result<Vec<AcceptedDocument>> {
    let mut parsed: Vec<AcceptedDocument> = Vec::new();
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for token in raw {
        let malformed = |detail: &str| {
            CliError::usage(format!("--accept-license {token:?}: {detail}")).with_fix(
                "pass the exact <id>@<version> token the plan prints, e.g. \
                 --accept-license univec-commercial@2026-08-09",
            )
        };
        let mut parts = token.split('@');
        let (id, version) = match (parts.next(), parts.next(), parts.next()) {
            (Some(id), Some(version), None) => (id, version),
            (_, None, _) => return Err(malformed("expected <id>@<version>")),
            _ => return Err(malformed("more than one `@`")),
        };
        if id.is_empty() {
            return Err(malformed("the licence id is empty"));
        }
        // The same rule the schema enforces on every index entry's licence
        // id, so every acknowledgeable document is expressible here and
        // vice versa.
        registry_schema::valid_license_id(id)
            .map_err(|e| malformed(&format!("bad licence id: {e}")))?;
        registry_schema::valid_license_version(version)
            .map_err(|e| malformed(&format!("bad version: {e}")))?;
        if !seen.insert((id.to_string(), version.to_string())) {
            return Err(CliError::usage(format!(
                "--accept-license {token}: the same document is acknowledged twice; each flag \
                 must appear once"
            )));
        }
        parsed.push(AcceptedDocument {
            license: id.to_string(),
            version: version.to_string(),
        });
    }
    Ok(parsed)
}

/// Per-model input to the terms plan: the entry that will actually be
/// downloaded or replaced, and — for a replacement — the receipt of the
/// installation being replaced, whose recorded evidence may satisfy the
/// same exact document without a new prompt.
pub struct TermsInput<'a> {
    pub model: &'a IndexModel,
    pub installed_receipt: Option<&'a Receipt>,
}

/// The built terms plan: the deterministic document groups for the plan's
/// `terms` block, plus evidence preserved from the receipts being replaced,
/// keyed by model name.
#[derive(Debug)]
pub struct TermsPlan {
    pub documents: Vec<TermsDocument>,
    pub preserved: BTreeMap<String, LicenseEvidence>,
}

/// Group the work items by exact document. Deterministic: documents sort by
/// (license, version, url, acceptance) and each lists its affected models
/// sorted — requested models, dependencies and postvec companions alike.
pub fn build(inputs: &[TermsInput<'_>]) -> Result<TermsPlan> {
    /// One exact document: (license id, version, URL, effective acceptance).
    type DocumentKey = (String, Option<String>, Option<String>, String);

    let mut groups: BTreeMap<DocumentKey, BTreeSet<String>> = BTreeMap::new();
    let mut preserved: BTreeMap<String, LicenseEvidence> = BTreeMap::new();
    let mut evidenced: BTreeMap<DocumentKey, bool> = BTreeMap::new();

    for input in inputs {
        let model = input.model;
        let Some(license) = &model.license else {
            // No terms id, no document — the entry still appears in the
            // ordinary plan steps.
            continue;
        };
        let acceptance = model.license_acceptance().to_string();
        if acceptance == "organization" {
            // Defensive: no valid production publisher/aphex path can serve
            // this today, and a local prompt must never stand in for the
            // server-side acceptance it demands.
            return Err(CliError::precondition(format!(
                "{}: its terms ({license}) require an organization-level acceptance that must \
                 be recorded server-side; this CLI cannot acknowledge it locally",
                model.name
            ))
            .with_fix("this entry is not installable until server-side acceptance exists"));
        }
        if acceptance == "notice"
            && (model.license_version.is_none() || model.license_url.is_none())
        {
            // Index validation forbids this; refuse rather than prompt for a
            // document that cannot be identified exactly.
            return Err(CliError::precondition(format!(
                "{}: a notice-policy document without an exact version and URL cannot be \
                 acknowledged",
                model.name
            )));
        }

        // Evidence recorded by the installation being replaced counts only
        // for the **exact** same document.
        let mut this_model_evidenced = false;
        if acceptance == "notice" {
            if let Some(receipt) = input.installed_receipt {
                let same_document = receipt.license.as_deref() == Some(license.as_str())
                    && receipt.license_version == model.license_version
                    && receipt.license_url == model.license_url;
                if same_document {
                    if let (Some(accepted_at), Some(method)) = (
                        &receipt.license_accepted_at,
                        &receipt.license_acceptance_method,
                    ) {
                        preserved.insert(
                            model.name.clone(),
                            LicenseEvidence {
                                accepted_at: accepted_at.clone(),
                                method: method.clone(),
                            },
                        );
                        this_model_evidenced = true;
                    }
                }
            }
        }

        let key = (
            license.clone(),
            model.license_version.clone(),
            model.license_url.clone(),
            acceptance,
        );
        groups
            .entry(key.clone())
            .or_default()
            .insert(model.name.clone());
        let entry = evidenced.entry(key).or_insert(true);
        *entry = *entry && this_model_evidenced;
    }

    let documents = groups
        .into_iter()
        .map(|((license, version, url, acceptance), models)| {
            let all_evidenced = *evidenced
                .get(&(
                    license.clone(),
                    version.clone(),
                    url.clone(),
                    acceptance.clone(),
                ))
                .expect("group was inserted");
            let required = acceptance == "notice" && !all_evidenced;
            let mut document = TermsDocument {
                license,
                version,
                url,
                acceptance,
                models: models.into_iter().collect(),
                acknowledgement_required: required,
                accept_flag: None,
            };
            if required {
                document.accept_flag = document
                    .token()
                    .map(|token| format!("--accept-license {token}"));
            }
            document
        })
        .collect();

    Ok(TermsPlan {
        documents,
        preserved,
    })
}

/// Resolve the acknowledgements a plan needs, before any download and before
/// the ordinary confirmation.
///
/// - Every supplied flag must name a document this plan actually requires an
///   acknowledgement for; anything else is stale and refused — dry run
///   included.
/// - On a dry run, nothing is asked and missing flags are fine: the plan
///   already printed them.
/// - Otherwise, each document requiring acknowledgement is satisfied by its
///   exact flag, or by one question to an interactive caller (`ask`, asked
///   once per distinct document). A non-interactive run missing any flag
///   fails with every exact token, before anything is downloaded or mutated.
///
/// Returns the freshly obtained evidence per (license, version).
pub fn resolve(
    documents: &[TermsDocument],
    flags: &[AcceptedDocument],
    dry_run: bool,
    prompt: Prompt,
    mut ask: impl FnMut(&TermsDocument) -> Result<bool>,
) -> Result<BTreeMap<(String, String), LicenseEvidence>> {
    // Staleness first, unconditionally: a flag that acknowledges nothing
    // this run needs must never be silently carried.
    for flag in flags {
        let applicable = documents.iter().any(|document| {
            document.license == flag.license
                && document.version.as_deref() == Some(flag.version.as_str())
                && document.acknowledgement_required
        });
        if applicable {
            continue;
        }
        let detail = match documents
            .iter()
            .find(|document| document.license == flag.license)
        {
            None => "no document with that licence id is in this plan".to_string(),
            Some(document) if document.version.as_deref() != Some(flag.version.as_str()) => {
                match document.token() {
                    Some(token) => format!(
                        "the plan carries version {:?} of that document — the exact flag is \
                         `--accept-license {token}`",
                        document.version.as_deref().unwrap_or("-")
                    ),
                    None => {
                        "that document is unversioned and has nothing to acknowledge".to_string()
                    }
                }
            }
            Some(document) if document.acceptance != "notice" => {
                "that document's policy requires no acknowledgement".to_string()
            }
            Some(_) => "its acknowledgement is already recorded for the installation being \
                        replaced"
                .to_string(),
        };
        return Err(CliError::usage(format!(
            "--accept-license {} is stale: {detail}; remove the flag",
            flag.token()
        )));
    }

    if dry_run {
        return Ok(BTreeMap::new());
    }

    let mut acknowledged: BTreeMap<(String, String), LicenseEvidence> = BTreeMap::new();
    let mut missing: Vec<&TermsDocument> = Vec::new();
    for document in documents {
        if !document.acknowledgement_required {
            continue;
        }
        let version = document
            .version
            .as_ref()
            .expect("a notice document is versioned")
            .clone();
        let by_flag = flags
            .iter()
            .any(|flag| flag.license == document.license && flag.version == version);
        if by_flag {
            acknowledged.insert(
                (document.license.clone(), version),
                LicenseEvidence {
                    accepted_at: crate::checks::timestamp_now(),
                    method: "flag".to_string(),
                },
            );
            continue;
        }
        if prompt.is_interactive() {
            if !ask(document)? {
                return Err(CliError::usage(format!(
                    "the terms document {} was not acknowledged; nothing was changed",
                    document.token().unwrap_or_else(|| document.license.clone())
                )));
            }
            acknowledged.insert(
                (document.license.clone(), version),
                LicenseEvidence {
                    accepted_at: crate::checks::timestamp_now(),
                    method: "interactive".to_string(),
                },
            );
            continue;
        }
        missing.push(document);
    }
    if !missing.is_empty() {
        let tokens: Vec<String> = missing
            .iter()
            .filter_map(|document| document.accept_flag.clone())
            .collect();
        return Err(CliError::usage(format!(
            "this run needs a terms acknowledgement that cannot be asked for without a \
             terminal; rerun with {} (each flag is version-specific; `--yes` answers only \
             the ordinary mutation confirmation and is still needed alongside)",
            tokens.join(" ")
        )));
    }
    Ok(acknowledged)
}

/// The evidence to record in one model's receipt: preserved from the
/// installation being replaced when the exact document was already
/// acknowledged there, otherwise the acknowledgement freshly obtained this
/// run. `None` for none-policy documents.
pub fn evidence_for(
    model: &IndexModel,
    preserved: &BTreeMap<String, LicenseEvidence>,
    acknowledged: &BTreeMap<(String, String), LicenseEvidence>,
) -> Option<LicenseEvidence> {
    if model.license_acceptance() != "notice" {
        return None;
    }
    if let Some(evidence) = preserved.get(&model.name) {
        return Some(evidence.clone());
    }
    let license = model.license.as_ref()?;
    let version = model.license_version.as_ref()?;
    acknowledged
        .get(&(license.clone(), version.clone()))
        .cloned()
}

/// The interactive acknowledgement: show the exact document — id, version,
/// canonical URL, affected models — and ask. What is recorded is that the
/// document was shown and acknowledged on this host, nothing more.
pub fn interactive_acknowledgement(document: &TermsDocument) -> Result<bool> {
    eprintln!(
        "\nThe following models are distributed under terms that must be shown before \
         download:"
    );
    for line in document.describe() {
        eprintln!("  {line}");
    }
    eprint!("Acknowledge these terms for this installation? [y/N] ");
    let answer = crate::plan::read_line()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::identity::Identity;
    use crate::registry::index::ArchiveInfo;

    fn model(name: &str, license: &str) -> IndexModel {
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
            license: Some(license.to_string()),
            license_version: None,
            license_url: None,
            license_acceptance: None,
            source: None,
            dependencies: vec![],
            postvec_requires: vec![],
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
                sources: vec!["https://bucket.s3/archives/x?X-Amz-Signature=SECRET".to_string()],
            },
        }
    }

    fn notice_model(name: &str, license: &str, version: &str) -> IndexModel {
        let mut model = model(name, license);
        model.license_version = Some(version.to_string());
        model.license_url = Some(format!(
            "https://univec.ai/legal/models/{license}/{version}"
        ));
        model.license_acceptance = Some("notice".to_string());
        model
    }

    fn identity() -> Identity {
        Identity {
            model_type: "embed".into(),
            backend: "onnx-runtime".into(),
            source_model: None,
            target_model: None,
            source_dim: None,
            target_dim: Some(384),
        }
    }

    fn receipt_for(model: &IndexModel, evidence: Option<&LicenseEvidence>) -> Receipt {
        Receipt::new(model, None, &[], &identity(), evidence)
    }

    fn inputs<'a>(models: &'a [IndexModel]) -> Vec<TermsInput<'a>> {
        models
            .iter()
            .map(|model| TermsInput {
                model,
                installed_receipt: None,
            })
            .collect()
    }

    fn flag(license: &str, version: &str) -> AcceptedDocument {
        AcceptedDocument {
            license: license.to_string(),
            version: version.to_string(),
        }
    }

    /// A closure that fails the test if the run asks anything.
    fn never_ask(document: &TermsDocument) -> Result<bool> {
        panic!("unexpected prompt for {}", document.license);
    }

    #[test]
    fn accept_license_tokens_parse_strictly() {
        let parsed = parse_accept_license(&[
            "univec-commercial@2026-08-09".to_string(),
            "mit@1".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed[0].license, "univec-commercial");
        assert_eq!(parsed[0].version, "2026-08-09");

        for bad in [
            "univec-commercial",  // no version
            "@2026-08-09",        // no id
            "univec-commercial@", // empty version
            "a@b@c",              // two separators
            "UPPER@1",            // hostile id
            "mit@1 2",            // whitespace in version
            "",                   // empty token
        ] {
            let err = parse_accept_license(&[bad.to_string()]).unwrap_err();
            assert!(err.to_string().contains("--accept-license"), "{bad}: {err}");
        }

        // Exact duplicates are refused; distinct versions of one id are not.
        let err = parse_accept_license(&["mit@1".to_string(), "mit@1".to_string()]).unwrap_err();
        assert!(err.to_string().contains("twice"), "{err}");
        parse_accept_license(&["mit@1".to_string(), "mit@2".to_string()]).unwrap();
    }

    /// Grouping is deterministic and by exact document, across requested
    /// models, dependencies and companions alike; the canonical document URL
    /// is shown, never an archive source.
    #[test]
    fn documents_group_deterministically_by_exact_document() {
        let models = vec![
            notice_model("z-converter", "univec-commercial", "2026-08-09"),
            model("b-embed", "apache-2.0"),
            model("a-bridge", "apache-2.0"),
            notice_model("a-converter", "univec-commercial", "2026-08-09"),
        ];
        let plan = build(&inputs(&models)).unwrap();
        assert_eq!(plan.documents.len(), 2);

        let apache = &plan.documents[0];
        assert_eq!(apache.license, "apache-2.0");
        assert_eq!(apache.acceptance, "none");
        assert_eq!(apache.models, ["a-bridge", "b-embed"]);
        assert!(!apache.acknowledgement_required);
        assert!(apache.accept_flag.is_none());

        let commercial = &plan.documents[1];
        assert_eq!(commercial.license, "univec-commercial");
        assert_eq!(commercial.version.as_deref(), Some("2026-08-09"));
        assert_eq!(commercial.models, ["a-converter", "z-converter"]);
        assert!(commercial.acknowledgement_required);
        assert_eq!(
            commercial.accept_flag.as_deref(),
            Some("--accept-license univec-commercial@2026-08-09")
        );

        // No presigned archive URL leaks into the terms output.
        let body = serde_json::to_string(&plan.documents).unwrap();
        assert!(!body.contains("X-Amz"), "{body}");
        assert!(body.contains("https://univec.ai/legal/models/univec-commercial/2026-08-09"));
    }

    /// `organization` cannot be satisfied locally: a defensive refusal, not
    /// a prompt.
    #[test]
    fn an_organization_policy_is_refused_defensively() {
        let mut org = notice_model("m", "univec-commercial", "1");
        org.license_acceptance = Some("organization".to_string());
        let models = vec![org];
        let err = build(&inputs(&models)).unwrap_err();
        assert!(err.to_string().contains("server-side"), "{err}");
    }

    /// None-policy documents never prompt — in any mode — and a flag
    /// supplied for one is stale.
    #[test]
    fn a_none_policy_never_prompts_and_its_flag_is_stale() {
        let models = vec![model("m", "mit")];
        let plan = build(&inputs(&models)).unwrap();
        for prompt in [Prompt::non_interactive(), Prompt::interactive()] {
            let acknowledged = resolve(&plan.documents, &[], false, prompt, never_ask).unwrap();
            assert!(acknowledged.is_empty());
        }
        let err = resolve(
            &plan.documents,
            &[flag("mit", "1")],
            false,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap_err();
        assert!(err.to_string().contains("stale"), "{err}");
        assert!(err.to_string().contains("unversioned"), "{err}");
    }

    /// Interactive notice: exactly one question per distinct document,
    /// however many models it covers; a declined prompt changes nothing.
    #[test]
    fn interactive_notice_prompts_once_per_document() {
        let models = vec![
            notice_model("a", "univec-commercial", "2026-08-09"),
            notice_model("b", "univec-commercial", "2026-08-09"),
        ];
        let plan = build(&inputs(&models)).unwrap();
        assert_eq!(plan.documents.len(), 1);

        let mut asked = 0;
        let acknowledged = resolve(
            &plan.documents,
            &[],
            false,
            Prompt::interactive(),
            |document| {
                asked += 1;
                assert_eq!(document.models, ["a", "b"]);
                Ok(true)
            },
        )
        .unwrap();
        assert_eq!(asked, 1);
        let evidence = &acknowledged[&("univec-commercial".to_string(), "2026-08-09".to_string())];
        assert_eq!(evidence.method, "interactive");

        let err = resolve(&plan.documents, &[], false, Prompt::interactive(), |_| {
            Ok(false)
        })
        .unwrap_err();
        assert!(err.to_string().contains("not acknowledged"), "{err}");
    }

    /// The exact flag satisfies a notice document without a prompt; `--yes`
    /// is not part of this decision at all (nothing here consults it).
    #[test]
    fn an_exact_flag_satisfies_notice_without_a_prompt() {
        let models = vec![notice_model("a", "univec-commercial", "2026-08-09")];
        let plan = build(&inputs(&models)).unwrap();
        for prompt in [Prompt::non_interactive(), Prompt::interactive()] {
            let acknowledged = resolve(
                &plan.documents,
                &[flag("univec-commercial", "2026-08-09")],
                false,
                prompt,
                never_ask,
            )
            .unwrap();
            let evidence =
                &acknowledged[&("univec-commercial".to_string(), "2026-08-09".to_string())];
            assert_eq!(evidence.method, "flag");
        }
    }

    /// A non-interactive run without the flags fails before anything is
    /// downloaded, printing every exact ready-to-paste flag and pointing at
    /// `--yes` for the separate mutation confirmation.
    #[test]
    fn a_non_interactive_refusal_prints_every_exact_flag() {
        let models = vec![
            notice_model("a", "univec-commercial", "2026-08-09"),
            notice_model("b", "special-terms", "2"),
        ];
        let plan = build(&inputs(&models)).unwrap();
        let err = resolve(
            &plan.documents,
            &[],
            false,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("--accept-license special-terms@2"), "{err}");
        assert!(
            err.contains("--accept-license univec-commercial@2026-08-09"),
            "{err}"
        );
        assert!(err.contains("--yes"), "{err}");

        // One flag present, one missing: still refused with the missing one.
        let err = resolve(
            &plan.documents,
            &[flag("special-terms", "2")],
            false,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("univec-commercial@2026-08-09"), "{err}");
    }

    /// Wrong version and unknown document are stale, and the wrong-version
    /// refusal names the exact correct flag.
    #[test]
    fn wrong_version_and_unknown_documents_are_stale() {
        let models = vec![notice_model("a", "univec-commercial", "2026-08-09")];
        let plan = build(&inputs(&models)).unwrap();

        let err = resolve(
            &plan.documents,
            &[flag("univec-commercial", "2025-01-01")],
            false,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("stale"), "{err}");
        assert!(
            err.contains("--accept-license univec-commercial@2026-08-09"),
            "{err}"
        );

        let err = resolve(
            &plan.documents,
            &[flag("ghost-terms", "1")],
            false,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no document"), "{err}");
    }

    /// Dry run: never prompts, missing flags are fine (the plan printed
    /// them), but stale flags still fail.
    #[test]
    fn a_dry_run_asks_nothing_and_still_refuses_stale_flags() {
        let models = vec![notice_model("a", "univec-commercial", "2026-08-09")];
        let plan = build(&inputs(&models)).unwrap();
        for prompt in [Prompt::non_interactive(), Prompt::interactive()] {
            let acknowledged = resolve(&plan.documents, &[], true, prompt, never_ask).unwrap();
            assert!(acknowledged.is_empty());
        }
        let err = resolve(
            &plan.documents,
            &[flag("univec-commercial", "wrong")],
            true,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap_err();
        assert!(err.to_string().contains("stale"), "{err}");
    }

    /// A replacement whose installed receipt already records the exact same
    /// document and acknowledgement does not prompt again, and its evidence
    /// is preserved into the new receipt; a changed document requires a new
    /// acknowledgement.
    #[test]
    fn a_same_document_upgrade_preserves_evidence_without_prompting() {
        let noticed = notice_model("a", "univec-commercial", "2026-08-09");
        let evidence = LicenseEvidence {
            accepted_at: "2026-08-01T00:00:00Z".to_string(),
            method: "interactive".to_string(),
        };
        let receipt = receipt_for(&noticed, Some(&evidence));

        let plan = build(&[TermsInput {
            model: &noticed,
            installed_receipt: Some(&receipt),
        }])
        .unwrap();
        assert!(!plan.documents[0].acknowledgement_required);
        assert!(plan.documents[0].accept_flag.is_none());
        let acknowledged = resolve(
            &plan.documents,
            &[],
            false,
            Prompt::interactive(),
            never_ask,
        )
        .unwrap();
        assert!(acknowledged.is_empty());
        assert_eq!(
            evidence_for(&noticed, &plan.preserved, &acknowledged),
            Some(evidence.clone())
        );
        // A flag for an already-evidenced document is stale.
        let err = resolve(
            &plan.documents,
            &[flag("univec-commercial", "2026-08-09")],
            false,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap_err();
        assert!(err.to_string().contains("already recorded"), "{err}");

        // The exact document changed: the old evidence does not carry.
        let newer = notice_model("a", "univec-commercial", "2026-09-01");
        let plan = build(&[TermsInput {
            model: &newer,
            installed_receipt: Some(&receipt),
        }])
        .unwrap();
        assert!(plan.documents[0].acknowledgement_required);
        assert!(plan.preserved.is_empty());

        // …and so does a receipt with the same document but no recorded
        // acknowledgement.
        let unacknowledged = receipt_for(&noticed, None);
        let plan = build(&[TermsInput {
            model: &noticed,
            installed_receipt: Some(&unacknowledged),
        }])
        .unwrap();
        assert!(plan.documents[0].acknowledgement_required);
    }

    /// A document evidenced for the replacement but also covering a freshly
    /// installed dependency still requires acknowledgement — the new install
    /// has no evidence of its own, and unrelated receipts are never scanned.
    #[test]
    fn a_new_dependency_needs_its_own_acknowledgement() {
        let upgraded = notice_model("upgraded", "univec-commercial", "2026-08-09");
        let evidence = LicenseEvidence {
            accepted_at: "2026-08-01T00:00:00Z".to_string(),
            method: "flag".to_string(),
        };
        let receipt = receipt_for(&upgraded, Some(&evidence));
        let fresh = notice_model("fresh-dep", "univec-commercial", "2026-08-09");

        let plan = build(&[
            TermsInput {
                model: &upgraded,
                installed_receipt: Some(&receipt),
            },
            TermsInput {
                model: &fresh,
                installed_receipt: None,
            },
        ])
        .unwrap();
        assert_eq!(plan.documents.len(), 1);
        assert!(plan.documents[0].acknowledgement_required);
        // The upgraded model still carries its preserved evidence; the fresh
        // dependency takes the new acknowledgement.
        assert_eq!(plan.preserved.get("upgraded"), Some(&evidence));
        let acknowledged = resolve(
            &plan.documents,
            &[flag("univec-commercial", "2026-08-09")],
            false,
            Prompt::non_interactive(),
            never_ask,
        )
        .unwrap();
        assert_eq!(
            evidence_for(&upgraded, &plan.preserved, &acknowledged),
            Some(evidence)
        );
        let fresh_evidence = evidence_for(&fresh, &plan.preserved, &acknowledged).unwrap();
        assert_eq!(fresh_evidence.method, "flag");
    }
}
