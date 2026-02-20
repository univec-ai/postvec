//! `postvec provider add`.
//!
//! Write (or extend) one providers.d file, verify the key with one live
//! single-input embed per model (opt-out `--no-verify` — the probe costs a
//! paid API call), and nudge the running host to reload.
//!
//! Two gates run before anything is written:
//!
//! - the ordinary confirmation, because this changes what a running host
//!   serves;
//! - the **privacy acknowledgement**: any existing column bound to a name
//!   this command makes live starts sending its source text to the provider
//!   on the next worker cycle, with no SQL change and no further notice
//!   (the bridge-upgrade event, external-providers §3.4). `--yes` never
//!   answers that; `--acknowledge-in-use` or the typed confirmation does.

use super::{
    columns_bound_to, read_secret_file, reload_host, require_private_secret_file,
    resolve_doc_secret, resolve_target, ProviderFileDoc, ProviderTarget,
};
use crate::cli::{Cli, ProviderAddArgs};
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use providers::catalog;
use std::path::PathBuf;
use std::time::Instant;

/// Connector types the factory accepts (plus the CLI aliases).
const KNOWN_TYPES: &[&str] = &[
    "openai",
    "openrouter",
    "mistral",
    "google",
    "gemini",
    "cohere",
    "aws",
    "amazon",
];

/// Where the key for this file comes from — written to the TOML verbatim
/// as a *source*, with the inline variant being the only one that stores a
/// value (in the 0600 file; documented as the least preferred).
enum KeySpec {
    File(PathBuf),
    Env(String),
    Inline(String),
    /// The existing file already carries one and no new source was given.
    Existing,
}

pub async fn run(cli: &Cli, args: ProviderAddArgs, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    // ---- Validate the request before any host access ----
    let typed = args.provider_type.to_lowercase();
    if !KNOWN_TYPES.contains(&typed.as_str()) {
        return Err(CliError::usage(format!(
            "unknown provider type {typed:?}; expected one of openai, openrouter, mistral, \
             google (alias: gemini), cohere, aws (alias: amazon)"
        )));
    }
    let canonical = catalog::canonical_provider(&typed);
    if canonical == "aws" && args.region.as_deref().unwrap_or("").trim().is_empty() {
        return Err(CliError::usage(
            "aws needs --region (the Bedrock region, e.g. us-east-1)",
        ));
    }
    if canonical != "aws" && args.region.is_some() {
        return Err(CliError::usage("--region only applies to the aws type"));
    }
    let stem = args.name.clone().unwrap_or_else(|| canonical.clone());
    validate_stem(&stem)?;

    let mut model_ids: Vec<String> = Vec::new();
    for id in &args.models {
        let id = id.trim().to_string();
        if id.is_empty() {
            return Err(CliError::usage("--model must not be empty"));
        }
        if !model_ids.contains(&id) {
            model_ids.push(id);
        }
    }
    if args.dim.is_some() && model_ids.len() != 1 {
        return Err(CliError::usage(
            "--dim applies to exactly one --model; add models with different dimensions in \
             separate runs",
        ));
    }

    // ---- Resolve the target and the existing file ----
    let mut target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    let file_path = target.dir().join(format!("{stem}.toml"));
    let existing = ProviderFileDoc::load(&file_path)?;
    if let Some(existing) = &existing {
        let existing_type = existing.provider_type().unwrap_or("");
        if catalog::canonical_provider(existing_type) != canonical {
            return Err(CliError::precondition(format!(
                "{} already configures provider type {existing_type:?}; pass --name to write \
                 a separate file for {canonical:?}",
                file_path.display()
            )));
        }
    }
    let already_declared: Vec<(String, String)> = existing
        .as_ref()
        .map(|doc| doc.models())
        .unwrap_or_default();

    // The descriptors this run adds: skip ids the file already declares.
    struct NewModel {
        id: String,
        public_name: String,
        dim: Option<u32>,
        max_tokens: Option<u32>,
        max_batch: Option<usize>,
    }
    let mut new_models: Vec<NewModel> = Vec::new();
    for id in &model_ids {
        if already_declared.iter().any(|(_, existing)| existing == id) {
            output.note(&format!(
                "{id} is already declared in {}",
                file_path.display()
            ));
            continue;
        }
        let public_name = catalog::public_name(&typed, id);
        if already_declared
            .iter()
            .any(|(name, _)| *name == public_name)
        {
            return Err(CliError::precondition(format!(
                "public name {public_name:?} is already declared in {} (for a different \
                 provider id)",
                file_path.display()
            )));
        }
        let known = catalog::lookup(&canonical, id);
        new_models.push(NewModel {
            id: id.clone(),
            public_name,
            dim: args.dim.or(known.map(|k| k.dim)),
            max_tokens: known.map(|k| k.max_tokens),
            max_batch: known.map(|k| k.max_batch),
        });
    }

    // ---- The key source ----
    let key = resolve_key_spec(&args, existing.is_some(), output)?;
    if let KeySpec::File(path) = &key {
        require_private_secret_file(path)?;
    }
    if let KeySpec::Env(var) = &key {
        output.note(&format!(
            "{var} must be present in the POSTMASTER's environment (or the postvec-server \
             unit's) — the inference host resolves it, not this shell; container images use \
             the POSTVEC_*/_FILE secret pattern"
        ));
    }

    // ---- Verification probe (and dimension inference) ----
    // A dry run never probes. The probe is a live, billed request that also
    // puts the key on the network, and `--dry-run` promises neither. The
    // plan says what the real run would do instead.
    let probe = !args.no_verify && !args.dry_run;
    if probe {
        let secret = probe_secret(&key, existing.as_ref())?;
        let config = providers::ProviderConfig {
            provider: canonical.clone(),
            api_key: (canonical != "aws").then(|| secret.clone()),
            base_url: args.base_url.clone(),
            region: args.region.clone(),
            bearer_token: (canonical == "aws").then(|| secret.clone()),
            access_key_id: None,
            secret_access_key: None,
        };
        for model in &mut new_models {
            let backend = providers::new_embedding_backend(
                &config,
                &model.id,
                model.dim.unwrap_or(0) as i32,
                "search_document",
                None,
            )
            .map_err(|e| CliError::precondition(format!("{}: {e}", model.id)))?;
            let deadline = std::time::Instant::now() + cli.timeout;
            let embeddings = backend
                .embed(&["postvec verification probe"], Some(deadline))
                .await
                .map_err(|e| {
                    CliError::precondition(format!(
                        "verification embed for {} failed: {e}",
                        model.id
                    ))
                    .with_fix(
                        "check the key, model id and network; pass --no-verify to write the \
                         file anyway (the probe costs one paid API call per model)",
                    )
                })?;
            let measured = embeddings.first().map(|e| e.vector.len()).unwrap_or(0) as u32;
            if measured == 0 {
                return Err(CliError::precondition(format!(
                    "verification embed for {} returned an empty vector",
                    model.id
                )));
            }
            match model.dim {
                Some(declared) if declared != measured => {
                    return Err(CliError::precondition(format!(
                        "{}: the probe returned {measured} dimensions but {declared} was \
                         declared; fix --dim (or drop it to use the measured value)",
                        model.id
                    )));
                }
                Some(_) => {}
                None => {
                    output.progress(&format!(
                        "{}: measured dimension {measured}",
                        model.public_name
                    ));
                    model.dim = Some(measured);
                }
            }
        }
    }
    for model in &new_models {
        // A dry run may legitimately reach here with no dimension yet: the
        // probe it skipped is what would have measured one. Only
        // --no-verify makes the gap permanent.
        if model.dim.is_none() && args.no_verify {
            return Err(CliError::usage(format!(
                "{} is not in the built-in catalog and --no-verify skips the probe; pass \
                 --dim <N> (its vector dimension) for it",
                model.id
            )));
        }
    }
    if args.dry_run && !args.no_verify && !new_models.is_empty() {
        output.note(
            "--dry-run: the verification embed was not sent. The real run makes one live \
             call per model, which the provider bills, and measures any dimension the \
             built-in catalog does not know",
        );
    }

    // ---- Privacy gate + plan ----
    let public_names: Vec<String> = new_models.iter().map(|m| m.public_name.clone()).collect();
    let scanned = matches!(target, ProviderTarget::Embedded { .. });
    let (columns, unknown_databases) = if scanned && !public_names.is_empty() {
        columns_bound_to(&mut target, &public_names, cli.timeout).await
    } else {
        (Vec::new(), Vec::new())
    };

    let mut plan = Plan::new("provider add", target.label());
    for name in &public_names {
        let mine: Vec<crate::plan::InUseColumn> = columns
            .iter()
            .filter(|column| &column.model == name)
            .cloned()
            .collect();
        if mine.is_empty() && unknown_databases.is_empty() {
            continue;
        }
        plan.push(PlanStep::AcknowledgeProviderPrivacy {
            provider: canonical.clone(),
            model: name.clone(),
            columns: mine,
            unknown_databases: unknown_databases.clone(),
        });
    }
    // Editing `base_url` or `region` on an existing file is a legitimate
    // reason to run this command with no new model and no new key (an Azure
    // front moves, a Bedrock deployment changes region). Without these two
    // terms the plan would be a no-op and the flag would be dropped in
    // silence.
    let recorded = |field: &str| {
        existing.as_ref().and_then(|doc| {
            doc.value
                .get(field)
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        })
    };
    let base_url_changes = args.base_url.is_some() && args.base_url != recorded("base_url");
    let region_changes = args.region.is_some() && args.region != recorded("region");
    if !new_models.is_empty()
        || matches!(key, KeySpec::File(_) | KeySpec::Env(_) | KeySpec::Inline(_))
        || base_url_changes
        || region_changes
    {
        plan.push(PlanStep::WriteConfig {
            path: file_path.clone(),
            before_sha256: existing.as_ref().map(|_| "existing".to_string()),
            after_sha256: "provider file".to_string(),
        });
    }
    if matches!(target, ProviderTarget::Path { .. }) && scanned_note_needed(&public_names) {
        output.note(
            "--path: no cluster is in scope, so columns bound to these names were not \
             checked — on the database side, `postvec provider add` without --path performs \
             that check",
        );
    }
    output.show_plan(&plan);

    if args.dry_run {
        let result = finish(
            &target,
            plan,
            ApplyJournal::default(),
            vec!["--dry-run: nothing was changed".to_string()],
            started,
            started_at,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }
    if plan.is_noop() {
        let result = finish(
            &target,
            plan,
            ApplyJournal::default(),
            vec![
                "nothing to do: every requested model is already declared, and no key, \
                 base URL or region changed"
                    .to_string(),
            ],
            started,
            started_at,
        );
        output.show_result(&result)?;
        return Ok(Exit::Success);
    }

    if args.acknowledge_in_use && plan.in_use_models().is_empty() {
        output.note(if scanned {
            "--acknowledge-in-use was not needed: no existing column is bound to these names"
        } else {
            "--acknowledge-in-use acknowledged nothing: with --path there is no cluster to \
             check, so no column was inspected"
        });
    }
    plan::confirm_in_use_with(
        &plan,
        args.acknowledge_in_use,
        args.yes,
        args.dry_run,
        Prompt::from_environment(),
        plan::interactive_provider_privacy_acknowledgement,
        "existing columns bound to {models} will start sending their source text to the \
         provider on the next worker cycle; pass --acknowledge-in-use together with --yes to \
         proceed knowingly. --yes deliberately does not stand in for it",
    )?;
    plan::confirm(&plan, args.yes, None, Prompt::from_environment())?;

    // ---- Apply: build the document, write, reload ----
    let mut journal = ApplyJournal::default();
    let mut doc = match existing {
        Some(doc) => doc,
        None => ProviderFileDoc {
            path: file_path.clone(),
            value: toml::Value::Table(Default::default()),
        },
    };
    {
        let table = doc.value.as_table_mut().expect("provider file is a table");
        table.insert("provider".into(), toml::Value::String(canonical.clone()));
        if let Some(region) = &args.region {
            table.insert("region".into(), toml::Value::String(region.clone()));
        }
        if let Some(base_url) = &args.base_url {
            table.insert("base_url".into(), toml::Value::String(base_url.clone()));
        }
        apply_key_spec(table, &canonical, &key);
    }
    for model in &new_models {
        let mut entry = toml::map::Map::new();
        entry.insert(
            "name".into(),
            toml::Value::String(model.public_name.clone()),
        );
        entry.insert(
            "provider_model_id".into(),
            toml::Value::String(model.id.clone()),
        );
        entry.insert(
            "dim".into(),
            toml::Value::Integer(model.dim.expect("dim resolved above") as i64),
        );
        if let Some(max_batch) = model.max_batch {
            entry.insert("max_batch".into(), toml::Value::Integer(max_batch as i64));
        }
        if let Some(max_tokens) = model.max_tokens {
            entry.insert("max_tokens".into(), toml::Value::Integer(max_tokens as i64));
        }
        doc.push_model(toml::Value::Table(entry));
    }

    doc.write(target.owner())?;
    journal.record(format!("wrote {} (0600)", file_path.display()));
    for model in &new_models {
        journal.record(format!(
            "{}: dim {}{}",
            model.public_name,
            model.dim.unwrap_or(0),
            if args.no_verify {
                " (unverified)"
            } else {
                " (verified)"
            }
        ));
        journal.succeeded(model.public_name.clone());
    }

    reload_host(&target, cli.timeout, &mut journal).await;

    let result = finish(&target, plan, journal, Vec::new(), started, started_at);
    output.show_result(&result)?;
    Ok(Exit::from_code(result.exit_code))
}

fn scanned_note_needed(public_names: &[String]) -> bool {
    !public_names.is_empty()
}

fn validate_stem(stem: &str) -> Result<()> {
    if stem.is_empty() || stem.len() > 64 {
        return Err(CliError::usage("--name must be 1..=64 characters"));
    }
    let first = stem.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(CliError::usage(format!(
            "--name {stem:?} must start with [a-z0-9]"
        )));
    }
    if !stem
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(CliError::usage(format!(
            "--name {stem:?} contains characters outside [a-z0-9._-]"
        )));
    }
    Ok(())
}

/// Which key source this run records. No flag + an existing keyed file keeps
/// the existing source; no flag + a TTY prompts hidden; no flag otherwise is
/// a usage error naming the three sources.
fn resolve_key_spec(args: &ProviderAddArgs, file_exists: bool, output: &Output) -> Result<KeySpec> {
    if let Some(path) = &args.api_key_file {
        let path = crate::validate::absolute_path(path, "--api-key-file")?;
        return Ok(KeySpec::File(path));
    }
    if let Some(var) = &args.api_key_env {
        if var.trim().is_empty() || var.contains(char::is_whitespace) {
            return Err(CliError::usage("--api-key-env must be a variable NAME"));
        }
        return Ok(KeySpec::Env(var.clone()));
    }
    if args.key_stdin {
        let raw = crate::proc::read_hidden_line("")
            .map_err(|e| CliError::apply(format!("cannot read the key from stdin: {e}")))?;
        return non_empty_key(raw);
    }
    if file_exists {
        return Ok(KeySpec::Existing);
    }
    if crate::proc::is_stdin_tty() && !output.is_json() {
        let raw = crate::proc::read_hidden_line("Provider API key (hidden): ")
            .map_err(|e| CliError::apply(format!("cannot read the key: {e}")))?;
        return non_empty_key(raw);
    }
    Err(CliError::usage(
        "no key source: pass --api-key-file FILE (recommended), --api-key-env VAR, or \
         --key-stdin — a key is never accepted as a command-line value",
    ))
}

fn non_empty_key(raw: String) -> Result<KeySpec> {
    let key = raw.trim().to_string();
    if key.is_empty() {
        return Err(CliError::usage("the key is empty"));
    }
    Ok(KeySpec::Inline(key))
}

/// Write the chosen source into the document with the type's field names
/// (AWS authenticates with the Bedrock bearer token here; the SigV4
/// static-pair variant is a hand-edit of the file's `access_key_id` /
/// `secret_access_key` triads, documented rather than flagged).
fn apply_key_spec(table: &mut toml::map::Map<String, toml::Value>, canonical: &str, key: &KeySpec) {
    let (inline, file, env) = if canonical == "aws" {
        ("bearer_token", "bearer_token_file", "bearer_token_env")
    } else {
        ("api_key", "api_key_file", "api_key_env")
    };
    let clear = |table: &mut toml::map::Map<String, toml::Value>| {
        table.remove(inline);
        table.remove(file);
        table.remove(env);
    };
    match key {
        KeySpec::File(path) => {
            clear(table);
            table.insert(file.into(), toml::Value::String(path.display().to_string()));
        }
        KeySpec::Env(var) => {
            clear(table);
            table.insert(env.into(), toml::Value::String(var.clone()));
        }
        KeySpec::Inline(value) => {
            clear(table);
            table.insert(inline.into(), toml::Value::String(value.clone()));
        }
        KeySpec::Existing => {}
    }
}

/// The secret value the verification probe uses. `--api-key-env` resolves
/// from *this* shell (the natural place a verifying operator has it);
/// `Existing` re-resolves whatever the file records.
fn probe_secret(key: &KeySpec, existing: Option<&ProviderFileDoc>) -> Result<String> {
    match key {
        KeySpec::Inline(value) => Ok(value.clone()),
        KeySpec::File(path) => read_secret_file(path),
        KeySpec::Env(var) => std::env::var(var).map_err(|_| {
            CliError::precondition(format!(
                "--api-key-env {var} is not set in this shell, so the key cannot be verified"
            ))
            .with_fix("export it here for the probe, or pass --no-verify")
        }),
        KeySpec::Existing => {
            let doc = existing.expect("Existing implies a loaded file");
            resolve_doc_secret(doc).map_err(|e| {
                e.with_fix(
                    "export the source in this shell for the probe, pass a new key source, \
                     or pass --no-verify",
                )
            })
        }
    }
}

fn finish(
    target: &ProviderTarget,
    plan: Plan,
    journal: ApplyJournal,
    mut messages: Vec<String>,
    started: Instant,
    started_at: String,
) -> CommandResult {
    let exit = journal.exit();
    for entry in &journal.applied {
        messages.push(format!("postvec: {entry}"));
    }
    for unfinished in &journal.incomplete {
        messages.push(format!("postvec: INCOMPLETE: {unfinished}"));
    }
    CommandResult {
        schema_version: crate::checks::SCHEMA_VERSION,
        command: plan.command,
        cli_version: crate::CLI_VERSION.to_string(),
        cluster: target.label(),
        started_at,
        duration_ms: started.elapsed().as_millis() as u64,
        plan,
        applied: journal.applied.clone(),
        messages,
        checks: Vec::new(),
        next_step: None,
        exit_code: exit.code(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stems_follow_the_file_name_rules() {
        assert!(validate_stem("openai").is_ok());
        assert!(validate_stem("openai-eu").is_ok());
        assert!(validate_stem("").is_err());
        assert!(validate_stem("Open AI").is_err());
        assert!(validate_stem("../escape").is_err());
        assert!(validate_stem(".hidden").is_err());
    }

    #[test]
    fn key_specs_write_the_right_fields_per_type() {
        let mut table = toml::map::Map::new();
        apply_key_spec(&mut table, "openai", &KeySpec::Env("OPENAI_API_KEY".into()));
        assert_eq!(
            table.get("api_key_env").and_then(toml::Value::as_str),
            Some("OPENAI_API_KEY")
        );
        // A new source replaces the previous one, whatever its kind.
        apply_key_spec(&mut table, "openai", &KeySpec::Inline("sk-x".into()));
        assert!(table.get("api_key_env").is_none());
        assert_eq!(
            table.get("api_key").and_then(toml::Value::as_str),
            Some("sk-x")
        );

        let mut aws = toml::map::Map::new();
        apply_key_spec(
            &mut aws,
            "aws",
            &KeySpec::File(PathBuf::from("/etc/postvec/keys/bedrock.key")),
        );
        assert_eq!(
            aws.get("bearer_token_file").and_then(toml::Value::as_str),
            Some("/etc/postvec/keys/bedrock.key")
        );
        assert!(aws.get("api_key_file").is_none(), "aws uses bearer fields");

        // Existing keeps whatever the file already records.
        let before = aws.clone();
        apply_key_spec(&mut aws, "aws", &KeySpec::Existing);
        assert_eq!(aws, before);
    }
}
