//! `postvec provider test` — the verification probe on demand: one live
//! single-input embed per configured model, dimensions checked against the
//! file. Costs one paid API call per model probed.

use super::{
    probe_one, resolve_doc_secret, resolve_target, validate_provider_name, ProviderFileDoc,
};
use crate::cli::{Cli, ProviderTestArgs};
use crate::error::{CliError, Exit, Result};
use crate::output::Output;
use providers::catalog;
use serde::Serialize;

#[derive(Serialize)]
struct ProbeResult {
    model: String,
    provider_model_id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    measured_dim: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_dim: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct TestDocument {
    schema_version: u32,
    command: &'static str,
    target: String,
    provider: String,
    results: Vec<ProbeResult>,
}

pub async fn run(cli: &Cli, args: ProviderTestArgs, output: &Output) -> Result<Exit> {
    validate_provider_name(&args.name, "NAME")?;
    let target = resolve_target(cli, args.path.as_deref(), output, false).await?;
    let file_path = target.dir().join(format!("{}.toml", args.name));
    let Some(doc) = ProviderFileDoc::load(&file_path)? else {
        return Err(CliError::precondition(format!(
            "no provider {:?} is configured ({} does not exist)",
            args.name,
            file_path.display()
        )));
    };
    // The serving host's own rules, before a paid call. Probing a file the
    // host refuses answers a question nobody asked: the model cannot serve
    // whatever the provider says, and a `dim = -1` or a typo'd field would
    // otherwise be discovered only after spending the call.
    match providers::config::validate_file(&file_path) {
        Ok(true) => {}
        Ok(false) => {
            return Err(CliError::precondition(format!(
                "{} is `enabled = false`, so the host serves nothing from it",
                file_path.display()
            ))
            .with_fix("set enabled = true to serve it, then rerun"))
        }
        Err(problem) => {
            return Err(CliError::precondition(format!(
                "the inference host would refuse {}: {problem}",
                file_path.display()
            ))
            .with_fix(
                "fix the file first — there is nothing to verify while the host will not \
                 load it",
            ))
        }
    }
    let provider_type = doc
        .provider_type()
        .ok_or_else(|| {
            CliError::precondition(format!("{} has no `provider` field", file_path.display()))
        })?
        .to_string();
    let canonical = catalog::canonical_provider(&provider_type);
    let secret = resolve_doc_secret(&doc)?;
    let field = |name: &str| {
        doc.value
            .get(name)
            .and_then(toml::Value::as_str)
            .map(str::to_string)
    };
    let config = providers::ProviderConfig {
        provider: canonical.clone(),
        api_key: (canonical != "aws").then(|| secret.clone()),
        base_url: field("base_url"),
        region: field("region"),
        bearer_token: (canonical == "aws").then(|| secret.clone()),
        // The CLI probe supports the bearer variant for AWS; a SigV4
        // static-pair file is probed by the serving host, not here.
        access_key_id: None,
        secret_access_key: None,
    };

    let models = doc.models();
    let selected: Vec<(String, String)> = match &args.model {
        Some(id_or_name) => {
            let matched: Vec<(String, String)> = models
                .iter()
                .filter(|(name, id)| name == id_or_name || id == id_or_name)
                .cloned()
                .collect();
            if matched.is_empty() {
                return Err(CliError::precondition(format!(
                    "{:?} declares no model {id_or_name:?}",
                    args.name
                )));
            }
            matched
        }
        None => models.clone(),
    };
    if selected.is_empty() {
        return Err(CliError::precondition(format!(
            "{:?} declares no models",
            args.name
        )));
    }

    let declared_dim = |name: &str| -> Option<i64> {
        doc.value
            .get("models")
            .and_then(toml::Value::as_array)
            .and_then(|list| {
                list.iter()
                    .find(|m| m.get("name").and_then(toml::Value::as_str) == Some(name))
            })
            .and_then(|m| m.get("dim"))
            .and_then(toml::Value::as_integer)
    };

    let mut results = Vec::new();
    let mut failed = false;
    for (name, id) in &selected {
        let declared = declared_dim(name);
        // The same probe `provider add` runs: one aligned, non-empty vector
        // and nothing less. `test`'s own copy accepted "the first vector,
        // whatever it is", so it reported success for responses the serving
        // gateway refuses.
        let outcome = probe_one(
            &config,
            id,
            declared.and_then(|d| u32::try_from(d).ok()),
            cli.timeout,
        )
        .await;
        match outcome {
            Ok(measured) => {
                let ok = declared.is_none_or(|d| d == measured as i64);
                if !ok {
                    failed = true;
                }
                results.push(ProbeResult {
                    model: name.clone(),
                    provider_model_id: id.clone(),
                    ok,
                    measured_dim: Some(measured as usize),
                    declared_dim: declared,
                    error: (!ok).then(|| {
                        format!(
                            "the probe returned {measured} dimensions but the file declares \
                             {}",
                            declared.unwrap_or_default()
                        )
                    }),
                });
            }
            Err(e) => {
                failed = true;
                results.push(ProbeResult {
                    model: name.clone(),
                    provider_model_id: id.clone(),
                    ok: false,
                    measured_dim: None,
                    declared_dim: declared,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    if output.is_json() {
        output.show_document(
            &TestDocument {
                schema_version: crate::checks::SCHEMA_VERSION,
                command: "provider test",
                target: target.label(),
                provider: args.name.clone(),
                results,
            },
            "",
        )?;
    } else {
        for result in &results {
            match (&result.error, result.measured_dim) {
                (None, Some(dim)) => output.progress(&format!("{}: OK (dim {dim})", result.model)),
                (Some(error), _) => output.progress(&format!("{}: FAIL — {error}", result.model)),
                _ => {}
            }
        }
    }
    Ok(if failed { Exit::Failure } else { Exit::Success })
}
