//! `postvec provider add`.
//!
//! Write (or extend) one providers.d file, verify the key with one live
//! single-input embed per model (opt-out `--no-verify` — the probe costs a
//! paid API call), and nudge the running host to reload. For `univec`,
//! names, kinds and dimensions come from the public catalogue instead
//! (`super::univec`), and the probe proves the key plus one added route per
//! kind rather than measuring every entry.
//!
//! Two gates run before anything is written:
//!
//! - the ordinary confirmation, because this changes what a running host
//!   serves;
//! - the **privacy acknowledgement**: any existing column bound to a name
//!   this command makes live starts sending its source text to the provider
//!   on the next worker cycle, with no SQL change and no further notice
//!   (the bridge-upgrade event). `--yes` never
//!   answers that; `--acknowledge-in-use` or the typed confirmation does.

use super::{
    columns_bound_to, read_secret_file, reload_host, require_private_secret_file,
    resolve_doc_secret, resolve_target, univec, validate_provider_name, write_secret_file,
    ProviderFileDoc, ProviderTarget,
};
use crate::cli::{Cli, ProviderAddArgs};
use crate::error::{CliError, Exit, Result};
use crate::output::{CommandResult, Output};
use crate::plan::{self, ApplyJournal, Plan, PlanStep, Prompt};
use providers::catalog;
use std::path::PathBuf;
use std::time::Instant;

/// Where the key for this file comes from — written to the TOML verbatim
/// as a *source*, with the inline variant being the only one that stores a
/// value (in the 0600 file; documented as the least preferred).
pub(super) enum KeySpec {
    File(PathBuf),
    Env(String),
    Inline(String),
    /// univec: the `postvec login` key, copied into `path` at apply time
    /// and referenced as `api_key_file` — never inline, so it cannot
    /// silently outlive a `logout`.
    /// `previous` is the file's old contents under `--replace-copied-key`,
    /// restored if the connector write fails.
    Login {
        key: String,
        path: PathBuf,
        previous: Option<String>,
    },
    /// The existing file already carries one and no new source was given.
    Existing,
}

pub async fn run(cli: &Cli, args: ProviderAddArgs, output: &Output) -> Result<Exit> {
    let started = Instant::now();
    let started_at = crate::checks::timestamp_now();

    // ---- Validate the request before any host access ----
    // The connector list is the loader's, not a second copy of it: the whole
    // point of a `provider add` refusal is that it predicts the host.
    let typed = args.provider_type.to_lowercase();
    let canonical = catalog::canonical_provider(&typed);
    if !providers::config::SUPPORTED_PROVIDERS.contains(&canonical.as_str()) {
        return Err(CliError::usage(format!(
            "unknown provider type {typed:?}; expected one of {} (aliases: gemini, amazon)",
            providers::config::SUPPORTED_PROVIDERS.join(", ")
        )));
    }
    if let Some(base_url) = &args.base_url {
        providers::config::validate_base_url(base_url).map_err(CliError::usage)?;
    }
    if canonical == "aws" {
        let region = args.region.as_deref().unwrap_or("").trim();
        if region.is_empty() {
            return Err(CliError::usage(
                "aws needs --region (the Bedrock region, e.g. us-east-1)",
            ));
        }
        // The same rule the loader enforces: the region becomes part of the
        // Bedrock hostname and of the SigV4 credential scope.
        providers::validate_region(region).map_err(CliError::usage)?;
        // The Titan connector builds its endpoint from the region alone and
        // never consults `base_url`. Writing one would be a setting the
        // operator can see in the file and the host silently ignores.
        if args.base_url.is_some() {
            return Err(CliError::usage(
                "--base-url does not apply to the aws type: the Bedrock endpoint is derived \
                 from --region",
            ));
        }
    }
    if canonical != "aws" && args.region.is_some() {
        return Err(CliError::usage("--region only applies to the aws type"));
    }
    let stem = args.name.clone().unwrap_or_else(|| canonical.clone());
    validate_provider_name(&stem, "--name")?;

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
    // Manual converter mode (`--convert-source`): one hosted converter
    // entry with every name and dimension spelled out. Validated as a unit
    // here so a partial flag set fails with one message naming the full
    // set; `Selectors::from_args` refuses `--model` and the catalogue
    // selectors beside it.
    let converter = converter_new_model(&args, &canonical)?;
    let selectors =
        univec::Selectors::from_args(&args, &canonical, converter.is_some(), model_ids.len())?;
    if args.dim.is_some()
        && converter.is_none()
        && (model_ids.len() + selectors.convert.len() != 1 || selectors.bulk())
    {
        return Err(CliError::usage(
            "--dim applies to exactly one --model or --convert; add models with different \
             dimensions in separate runs",
        ));
    }

    // ---- Resolve the target and the existing file ----
    let mut target = resolve_target(cli, args.path.as_deref(), output, true).await?;
    // Held for the whole read-modify-write. Two concurrent `provider add`
    // runs would otherwise both load the file, both append, and the second
    // rename would silently discard the first one's model.
    let _host_lock = if args.dry_run {
        None
    } else {
        crate::config::owned::host_lock_shared()?
    };
    let _lock = (!args.dry_run)
        .then(|| super::lock_provider_dir(target.dir(), target.owner()))
        .transpose()?;
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
    // `existing` is consumed when the prospective document is built; the
    // probe still needs the file's own key source and declared dimensions.
    let existing_for_probe = existing.as_ref().map(|doc| ProviderFileDoc {
        path: doc.path.clone(),
        value: doc.value.clone(),
    });
    // Entries the file already declares as CONVERTERS, by public name.
    // They are excluded from embed re-probes (a converter cannot answer an
    // embed call) and from the text-egress privacy names below.
    let existing_converters: std::collections::BTreeSet<String> = existing_for_probe
        .as_ref()
        .map(|doc| {
            doc.descriptors()
                .into_iter()
                .filter(|d| d.kind == providers::config::ModelKind::Convert)
                .map(|d| d.name)
                .collect()
        })
        .unwrap_or_default();

    // ---- The key source ----
    // Before the catalogue: an interactive run's prompt order is key, then
    // discovery progress — not the other way round.
    let key = resolve_key_spec(
        &args,
        &canonical,
        &stem,
        target.dir(),
        existing.is_some(),
        output,
    )?;
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

    // Read the effective values *from the document*, not from the flags: a
    // rerun that adds a model to an existing file with a custom `base_url`
    // does not repeat `--base-url`, and everything downstream — the
    // catalogue, the probe, the privacy scan — has to reason about the
    // endpoint that will serve, not the one the command line mentioned.
    let recorded = |field: &str| {
        existing.as_ref().and_then(|doc| {
            doc.value
                .get(field)
                .and_then(toml::Value::as_str)
                .map(str::to_string)
        })
    };
    let effective_base_url = args.base_url.clone().or_else(|| recorded("base_url"));
    let effective_region = args.region.clone().or_else(|| recorded("region"));
    let base_url_changes = args.base_url.is_some() && args.base_url != recorded("base_url");
    let region_changes = args.region.is_some() && args.region != recorded("region");

    // ---- UniVec's catalogue ----
    // Unauthenticated and unbilled, so a dry run fetches it too. `None` means
    // "not consulted": another connector, --no-catalog, or unreachable
    // with selectors that can do without it.
    let catalogue: Option<Vec<providers::listing::ListedModel>> =
        if canonical == "univec" && !args.no_catalog {
            let required = selectors.needs_catalogue(model_ids.len(), converter.is_some());
            univec::fetch_catalogue(effective_base_url.as_deref(), cli.timeout, required, output)
                .await?
        } else {
            None
        };
    if let Some(catalogue) = &catalogue {
        if selectors.needs_catalogue(model_ids.len(), converter.is_some()) && !selectors.bulk() {
            // No selector at all: every embed model UniVec lists. Converters
            // are pair-specific migration routes and stay opt-in.
            model_ids = catalogue
                .iter()
                .filter(|m| m.kind == providers::listing::ListedKind::Embed)
                .map(|m| m.provider_model_id.clone())
                .collect();
        }
    }

    // The descriptors this run adds: skip ids the file already declares.
    let mut new_models: Vec<NewModel> = Vec::new();
    let mut already_present = 0usize;
    for id in &model_ids {
        if already_declared.iter().any(|(_, existing)| existing == id) {
            output.note(&format!(
                "{id} is already declared in {}",
                file_path.display()
            ));
            already_present += 1;
            continue;
        }
        let public_name = catalog::public_name(&typed, id);
        // The derivation reduces any id to the loader's charset, so this
        // normally cannot fire. It stays because the consequence if it ever
        // did is disproportionate: the host refuses a connector file *as a
        // whole* over one bad model name, so a name that slipped through
        // would take that provider's already-working models down at the
        // next reload rather than just failing to add this one.
        catalog::validate_public_name(&public_name).map_err(|e| {
            CliError::usage(format!("--model {id:?}: {e}"))
                .with_fix("give the model an id with letters or digits in it")
        })?;
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
        let listed = catalogue
            .as_deref()
            .and_then(|c| providers::listing::embed(c, id));
        if catalogue.is_some() && listed.is_none() {
            output.note(&format!(
                "{id} is not in UniVec's catalogue; the probe will measure its dimension"
            ));
        }
        if let (Some(listed), Some(dim)) = (listed, args.dim) {
            if dim != listed.dim {
                return Err(CliError::usage(format!(
                    "--dim {dim} disagrees with UniVec's catalogue, which lists {id} at \
                     {} dimensions; drop --dim",
                    listed.dim
                )));
            }
        }
        new_models.push(NewModel {
            id: id.clone(),
            placeholder_dim: providers::config::placeholder_dim(&canonical, id),
            public_name,
            dim: args.dim.or(listed.map(|l| l.dim)).or(known.map(|k| k.dim)),
            max_tokens: listed
                .and_then(|l| l.sequence_len)
                .or(known.map(|k| k.max_tokens)),
            max_batch: known.map(|k| k.max_batch),
            catalogued: listed.is_some(),
            convert: None,
        });
    }
    // Converters: the manual entry, or the catalogue selection. A rerun
    // with the same route is a no-op note, exactly like an embed id the
    // file already declares; the same name with a DIFFERENT route is a
    // refusal — which entry wins would decide what a migration converts
    // through.
    let mut converters: Vec<NewModel> = converter.into_iter().collect();
    if let Some(catalogue) = &catalogue {
        for listed in selectors.converters(catalogue)? {
            let mut model =
                univec::converter_model(listed, catalogue, args.converter_name.as_deref())?;
            if let Some(dim) = args.dim {
                if dim != model.dim.unwrap_or(dim) {
                    return Err(CliError::usage(format!(
                        "--dim {dim} disagrees with UniVec's catalogue, which lists {} at {} \
                         dimensions; drop --dim",
                        model.public_name,
                        model.dim.unwrap_or(0)
                    )));
                }
                model.dim = Some(dim);
            }
            converters.push(model);
        }
    }
    for converter in converters {
        let prospective_route = converter_route_id(&converter);
        let existing_route = existing_for_probe.as_ref().and_then(|doc| {
            doc.descriptors()
                .into_iter()
                .find(|d| d.name == converter.public_name)
                .map(|d| d.route_model_id())
        });
        match existing_route {
            Some(route) if route == prospective_route => {
                output.note(&format!(
                    "{} is already declared in {}",
                    converter.public_name,
                    file_path.display()
                ));
                already_present += 1;
            }
            Some(_) => {
                return Err(CliError::precondition(format!(
                    "public name {:?} is already declared in {} with a different route; give \
                     this converter its own --converter-name",
                    converter.public_name,
                    file_path.display()
                )))
            }
            None if already_declared
                .iter()
                .any(|(name, _)| *name == converter.public_name) =>
            {
                return Err(CliError::precondition(format!(
                    "public name {:?} is already declared in {}",
                    converter.public_name,
                    file_path.display()
                )))
            }
            None => new_models.push(converter),
        }
    }
    // A selection over the per-file ceiling is refused as a whole, never
    // truncated; the loader would refuse the file anyway, but its message
    // cannot name the flags that narrow a catalogue selection.
    if already_declared.len() + new_models.len() > providers::config::MAX_MODELS_PER_FILE {
        return Err(CliError::precondition(format!(
            "{} entries would exceed the {} per-file ceiling",
            already_declared.len() + new_models.len(),
            providers::config::MAX_MODELS_PER_FILE
        ))
        .with_fix("narrow the selection with --convert-to / --convert-from / --model, or write a second file with --name"));
    }

    // ---- Whether a verification probe will run ----
    // The probe is deferred until after both confirmation gates below. It
    // is a live, billed request that puts the key on the network, and until
    // the operator has answered the plan — the privacy acknowledgement in
    // particular — this command has no mandate to spend either. A dry run
    // never probes at all; the plan says what the real run would do.
    let probe = !args.no_verify && !args.dry_run;
    for model in &new_models {
        // A dry run may legitimately reach here with no dimension yet: the
        // probe it skipped is what would have measured one. Only
        // --no-verify makes the gap permanent.
        if model.dim.is_none() && args.no_verify {
            return Err(CliError::usage(if model.convert.is_some() {
                format!(
                    "--no-verify skips the probe that would measure the converter's target \
                     dimension; pass --dim <N> for {}",
                    model.public_name
                )
            } else {
                format!(
                    "{} is not in the built-in catalog and --no-verify skips the probe; pass \
                     --dim <N> (its vector dimension) for it",
                    model.id
                )
            }));
        }
    }
    if args.dry_run && !args.no_verify && !new_models.is_empty() {
        output.note(
            "--dry-run: the verification embed was not sent. The real run makes one live \
             call per model, which the provider bills, and measures any dimension the \
             built-in catalog does not know",
        );
    }

    // ---- What this run actually changes ----
    // Moving base_url or region on an existing file is a recipient change:
    // same public names, different destination. The privacy gate must
    // cover those names, not only newly added models. A brand-new file is
    // an ordinary add over its new names.
    let endpoint_changes = existing.is_some() && (base_url_changes || region_changes);
    // A new key source for the same endpoint is not a recipient change: the
    // text goes to the same place. It does change what the host will do, so
    // it is worth *verifying*, but it must not demand a privacy
    // acknowledgement — over-prompting is how a gate stops being read.
    let credential_changes = !matches!(key, KeySpec::Existing);

    // ---- Privacy gate + plan ----
    // Text-egress names only: a converter's own name never binds a column
    // and no source text flows through it — vectors do, and only when an
    // operator explicitly calls postvec.migrate()/convert(), which is where
    // its consent moment lives (the migrate-time NOTICE naming the
    // provider). Putting converter names through this gate would make the
    // acknowledgement assert something false.
    let new_names: Vec<String> = new_models
        .iter()
        .filter(|m| m.convert.is_none())
        .map(|m| m.public_name.clone())
        .collect();
    // Every name this file will serve, when the recipient moves; only the
    // new ones otherwise.
    let public_names: Vec<String> = if endpoint_changes {
        let mut all: Vec<String> = already_declared
            .iter()
            .filter(|(n, _)| !existing_converters.contains(n))
            .map(|(n, _)| n.clone())
            .collect();
        all.extend(new_names.iter().cloned());
        all
    } else {
        new_names.clone()
    };
    let scanned = matches!(target, ProviderTarget::Embedded { .. });
    let (columns, unknown_databases) = if scanned && !public_names.is_empty() {
        columns_bound_to(&mut target, &public_names, cli.timeout).await
    } else {
        (Vec::new(), Vec::new())
    };
    // A file parked with `enabled = false` serves nothing, so nothing starts
    // being sent anywhere: no privacy step, or the plan would warn about a
    // recipient that does not exist. The journal says the file is parked.
    let file_parked = existing.as_ref().is_some_and(|doc| !doc.enabled());

    let mut plan = Plan::new("provider add", target.label());
    for name in public_names.iter().filter(|_| !file_parked) {
        let mine: Vec<crate::plan::InUseColumn> = columns
            .iter()
            .filter(|column| &column.model == name)
            .cloned()
            .collect();
        // `--path` has no cluster, so nothing here can list the affected
        // columns. That is not the same as there being none, and treating an
        // unanswerable question as a clean answer is the one outcome a
        // privacy gate must never produce — so the step is pushed anyway,
        // with the databases marked UNKNOWN. `--yes` is not an answer to "may
        // this text go somewhere else"; the step is what makes
        // `--acknowledge-in-use` (or the typed confirmation) the way through.
        //
        // Every name this run makes newly live, not only an endpoint move: a
        // *first* `provider add --path openai --model …` on a fleet node is
        // exactly the bridge-upgrade event — a column already bound to that
        // public name and served through a converter starts being embedded by
        // the provider on the next worker cycle. On a cluster target the scan
        // answers that question; here nothing can, and `--path` is the
        // documented way to administer remote nodes, so it must not be the
        // one mode with no gate.
        let unknown = if !scanned {
            vec!["every database served by this node (not inspectable from --path)".to_string()]
        } else {
            unknown_databases.clone()
        };
        if mine.is_empty() && unknown.is_empty() {
            continue;
        }
        plan.push(PlanStep::AcknowledgeProviderPrivacy {
            provider: canonical.clone(),
            model: name.clone(),
            columns: mine,
            unknown_databases: unknown,
        });
    }
    if endpoint_changes && !scanned && !public_names.is_empty() {
        output.note(
            "--path: this changes where source text is SENT for every model in this file, and \
             no cluster is in scope to list the bound columns. Check `postvec.registry` on the \
             database hosts that use this node",
        );
    }
    // Editing `base_url` or `region` on an existing file is a legitimate
    // reason to run this command with no new model and no new key (an Azure
    // front moves, a Bedrock deployment changes region). Without these two
    // terms the plan would be a no-op and the flag would be dropped in
    // silence.
    // The copied login key is its own step: a credential file appearing
    // (or being rewritten) is something the operator confirms by name.
    if let KeySpec::Login { path, .. } = &key {
        plan.push(PlanStep::WriteConfig {
            path: path.clone(),
            before_sha256: path.exists().then(|| "existing".to_string()),
            after_sha256: "copied login key".to_string(),
        });
    }
    if !new_models.is_empty() || credential_changes || endpoint_changes {
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
    // Prospective document, checked before anything is spent. Compose it
    // here so the whole file, not just the model ids the probe touches, is
    // measured against the serving host's rules before a paid call. An
    // unknown field, two key sources, a `dim` outside a model's range: all
    // of those make the host refuse the file. Catch them before the probe.
    //
    // Models whose dimension the probe has yet to measure are appended
    // afterwards; everything else about the file is final by this point.
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
    // Include models whose dim is still a placeholder so structural rules
    // (file count, id length, duplicates) still see them. The probe then
    // replaces the placeholder.
    for model in &new_models {
        doc.push_model(model_entry(model))?;
    }
    // `--dry-run` has to answer the same question the real run would, so
    // this runs before it returns: a dry run that said "fine" about a file
    // the host refuses was a prediction of nothing.
    let file_enabled = doc.validate_prospective()?;
    if !file_enabled {
        output.note(&format!(
            "{} is `enabled = false`: the file is written correctly and the host serves \
             nothing from it until that changes",
            file_path.display()
        ));
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
            "--acknowledge-in-use had no effect: with --path there is no cluster to check, \
             and no column was inspected"
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

    // ---- Verification probe (and dimension inference) ----
    // Deliberately after both gates: the first thing this command does that
    // costs money and sends the key over the network happens only once the
    // operator has said yes to the plan. Nothing has been written yet, so a
    // probe failure still leaves the host exactly as it was.
    if probe {
        let secret = probe_secret(&key, existing_for_probe.as_ref())?;
        // The configuration the *host* will serve with, not the one the
        // command line mentioned. Probing `args.base_url` meant that adding a
        // model to a file with a custom endpoint verified the public default
        // and then wrote and reloaded the custom one — a green check for a
        // request that was never made against the endpoint that matters.
        let config = providers::ProviderConfig {
            provider: canonical.clone(),
            api_key: (canonical != "aws").then(|| secret.clone()),
            base_url: effective_base_url.clone(),
            region: effective_region.clone(),
            bearer_token: (canonical == "aws").then(|| secret.clone()),
            access_key_id: None,
            secret_access_key: None,
        };

        // What gets a live call. An additive run verifies what it adds. A
        // change to the *connector* — a rotated key, a moved endpoint —
        // changes what every model in the file does, and verifying none of
        // them (which is what "probe the new models" meant for a run that
        // adds none) let a key rotation report success without a single
        // request. That is the failure this command exists to prevent.
        // Embed probes only: a converter cannot answer an embed call, so
        // converter entries — the new one and any the file already declares
        // — go through the convert probe below instead.
        let already_embed: Vec<(String, String)> = already_declared
            .iter()
            .filter(|(name, _)| !existing_converters.contains(name))
            .cloned()
            .collect();
        // UniVec: a best-effort unbilled identity check before anything is
        // billed. A 401/403 stops here; anything else defers to the probe.
        if canonical == "univec" {
            univec::identity_check(effective_base_url.as_deref(), &secret, cli.timeout, output)
                .await?;
        }
        // Catalogue-sourced entries have their dimensions; the probe's job
        // is to prove the key and that one added route of each kind
        // serves — one billed call per kind, against something THIS run
        // adds (the cheapest embed among them), never a catalogue member
        // outside the selection. A width that disagrees with the catalogue
        // is refused, not patched.
        if let Some(model) = new_models
            .iter()
            .filter(|m| m.catalogued && m.convert.is_none())
            .min_by_key(|m| m.max_tokens.unwrap_or(u32::MAX))
        {
            let measured = super::probe_one(&config, &model.id, None, cli.timeout).await?;
            catalogue_agrees(&model.public_name, model.dim, measured)?;
            output.progress(&format!(
                "{}: verified ({measured} dimensions)",
                model.public_name
            ));
        }
        if let Some(model) = new_models
            .iter()
            .find(|m| m.catalogued && m.convert.is_some())
        {
            let descriptor = probe_descriptor(model).expect("a converter");
            let measured = super::probe_convert_one(&config, &descriptor, cli.timeout).await?;
            catalogue_agrees(&model.public_name, model.dim, measured)?;
            output.progress(&format!(
                "{}: verified ({measured} target dimensions)",
                model.public_name
            ));
        }
        for (id, public_name, declared) in probe_targets(
            &new_models,
            &already_embed,
            existing_for_probe.as_ref(),
            credential_changes || endpoint_changes,
        ) {
            let measured = super::probe_one(&config, &id, declared, cli.timeout).await?;
            match declared {
                Some(declared) if declared != measured => {
                    return Err(CliError::precondition(format!(
                        "{id}: the probe returned {measured} dimensions but {declared} was \
                         declared; fix --dim (or drop it to use the measured value)"
                    )));
                }
                Some(_) => {}
                None => {
                    output.progress(&format!("{public_name}: measured dimension {measured}"));
                    if let Some(model) = new_models.iter_mut().find(|m| m.id == id) {
                        model.dim = Some(measured);
                    }
                }
            }
        }

        // The manual converter entry, always. Its measured target dimension
        // patches an omitted --dim, exactly like an embed probe's.
        for model in new_models.iter_mut().filter(|m| !m.catalogued) {
            let Some(descriptor) = probe_descriptor(model) else {
                continue;
            };
            let measured = super::probe_convert_one(&config, &descriptor, cli.timeout).await?;
            match model.dim {
                Some(declared) if declared != measured => {
                    return Err(CliError::precondition(format!(
                        "{}: the probe returned {measured} target dimensions but {declared} \
                         was declared; fix --dim (or drop it to use the measured value)",
                        model.public_name
                    )));
                }
                Some(_) => {}
                None => {
                    output.progress(&format!(
                        "{}: measured target dimension {measured}",
                        model.public_name
                    ));
                    model.dim = Some(measured);
                }
            }
        }
        // Every already-declared converter too, when the connector itself
        // changed — the same rule as the embed re-probe list, for the same
        // reason: a rotated key or moved endpoint changes what every entry
        // in the file does.
        if credential_changes || endpoint_changes {
            if let Some(doc) = existing_for_probe.as_ref() {
                for descriptor in doc
                    .descriptors()
                    .into_iter()
                    .filter(|d| d.kind == providers::config::ModelKind::Convert)
                {
                    let measured =
                        super::probe_convert_one(&config, &descriptor, cli.timeout).await?;
                    if measured != descriptor.dim {
                        return Err(CliError::precondition(format!(
                            "{}: the probe returned {measured} target dimensions but the file \
                             declares {}; the conversion route no longer produces what the \
                             file says",
                            descriptor.name, descriptor.dim
                        )));
                    }
                }
            }
        }
    }

    // ---- Apply: finish the document, write, reload ----
    // Every entry is already present and already validated; only the
    // dimensions the probe measured are still placeholders. `write` runs the
    // same rules once more over the finished file, with the real values.
    let mut journal = ApplyJournal::default();
    for model in &new_models {
        if let Some(dim) = model.dim {
            doc.set_model_dim(&model.public_name, dim)?;
        }
    }
    if let KeySpec::Login {
        key,
        path,
        previous,
    } = &key
    {
        write_secret_file(path, key.as_bytes(), target.owner())?;
        journal.record(format!(
            "{} the postvec login key {} {} (0600); `postvec logout` does not remove it",
            if previous.is_some() {
                "rotated"
            } else {
                "copied"
            },
            if previous.is_some() { "in" } else { "to" },
            path.display()
        ));
        if let (Some(old), Err(e)) = (previous, doc.write(target.owner())) {
            // The connector still says the old key; put it back.
            write_secret_file(path, old.as_bytes(), target.owner())?;
            return Err(e);
        }
    }
    if !matches!(
        key,
        KeySpec::Login {
            previous: Some(_),
            ..
        }
    ) {
        doc.write(target.owner())?;
    }
    journal.record(format!("wrote {} (0600)", file_path.display()));
    for model in &new_models {
        let shape = match &model.convert {
            Some(convert) => format!(
                "converts {}[{}] -> {}[{}]",
                convert.source_model,
                convert.source_dim,
                convert.target_model,
                model.dim.unwrap_or(0)
            ),
            None => format!("dim {}", model.dim.unwrap_or(0)),
        };
        journal.record(format!(
            "{}: {shape}{}{}",
            model.public_name,
            if args.no_verify {
                " (unverified)"
            } else {
                " (verified)"
            },
            if file_enabled {
                ""
            } else {
                " — NOT served: the file is enabled = false"
            }
        ));
        journal.succeeded(model.public_name.clone());
    }
    // A parked file is the operator's choice and the write completed, so the
    // exit is success — `incomplete` (partial) would claim the command did
    // not finish, which is false. The machine-readable statement goes in
    // `next_step`, the field that exists for "this is done; here is what
    // serves it": a caller checks one field rather than parsing prose.
    let next_step = (!file_enabled).then(|| {
        format!(
            "set enabled = true in {} to serve the {} model(s) written to it",
            file_path.display(),
            new_models.len()
        )
    });

    if catalogue.is_some() {
        let embeds = new_models.iter().filter(|m| m.convert.is_none()).count();
        journal.record(format!(
            "catalogue: {embeds} embed, {} convert added, {already_present} already present",
            new_models.len() - embeds
        ));
        if !selectors.any() {
            journal.record(
                "conversion routes are opt-in: `postvec provider add univec --convert-to <target>`"
                    .to_string(),
            );
        } else if embeds == 0 {
            journal.record(
                "search after migrate() still needs a local embed model of the target (or a bridge); \
                 a converter serves no embedding"
                    .to_string(),
            );
        }
    }

    reload_host(&target, cli.timeout, &mut journal).await;
    // The host now serves the model; the databases do not know it exists.
    // `enable()` resolves through `postvec.models`, so without this the
    // documented `provider add` → `enable` sequence fails until the worker's
    // next discovery cycle (up to a minute plus jitter). A `--path` target
    // has no cluster and is skipped inside.
    super::refresh_databases(&mut target, &mut journal).await;

    let mut result = finish(&target, plan, journal, Vec::new(), started, started_at);
    result.next_step = next_step;
    output.show_result(&result)?;
    Ok(Exit::from_code(result.exit_code))
}

fn scanned_note_needed(public_names: &[String]) -> bool {
    !public_names.is_empty()
}

/// One descriptor this run is adding.
pub(super) struct NewModel {
    /// The provider-side id: an embed model's own, a converter's TARGET.
    pub id: String,
    pub public_name: String,
    /// An embed model's width; a converter's TARGET width.
    pub dim: Option<u32>,
    /// Stands in for `dim` during pre-probe validation only.
    pub placeholder_dim: u32,
    pub max_tokens: Option<u32>,
    pub max_batch: Option<usize>,
    /// Dimensions came from UniVec's catalogue: the probe proves the key
    /// and one route per kind rather than measuring every entry.
    pub catalogued: bool,
    /// `Some` makes this a `kind = "convert"` entry.
    pub convert: Option<ConvertSpec>,
}

/// The converter half of a converter entry.
pub(super) struct ConvertSpec {
    /// Provider-side id of the SOURCE space.
    pub provider_source_id: String,
    /// Postvec-side public name of the source space (the resolver's
    /// vocabulary — what a bound column's `model` says).
    pub source_model: String,
    /// Postvec-side public name of the target space.
    pub target_model: String,
    pub source_dim: u32,
}

/// A catalogue dimension the probe contradicts is a refusal with both
/// numbers; patching would write what the catalogue denies.
fn catalogue_agrees(name: &str, declared: Option<u32>, measured: u32) -> Result<()> {
    match declared {
        Some(declared) if declared != measured => Err(CliError::precondition(format!(
            "{name}: UniVec's catalogue lists {declared} dimensions but the probe measured \
             {measured}; nothing was written"
        ))
        .with_fix("retry later, or add the entry manually with --no-catalog and --dim")),
        _ => Ok(()),
    }
}

/// The `--convert-*` flags as one converter entry, or `None` when the run
/// adds embed models. All five are validated as a unit; `--converter-name`
/// defaults to the documented derived spelling.
fn converter_new_model(args: &ProviderAddArgs, canonical: &str) -> Result<Option<NewModel>> {
    let any = args.convert_source.is_some()
        || args.convert_target.is_some()
        || args.source_model.is_some()
        || args.target_model.is_some()
        || args.source_dim.is_some();
    if !any {
        return Ok(None);
    }
    if canonical != "univec" {
        return Err(CliError::usage(
            "--convert-source adds a hosted converter, which only provider \"univec\" serves",
        ));
    }
    let require = |value: &Option<String>, flag: &str| -> Result<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                CliError::usage(format!(
                    "a converter needs {flag}: pass --convert-source, --convert-target, \
                     --source-model, --target-model and --source-dim together"
                ))
            })
    };
    let provider_source_id = require(&args.convert_source, "--convert-source")?;
    let provider_target_id = require(&args.convert_target, "--convert-target")?;
    let source_model = require(&args.source_model, "--source-model")?;
    let target_model = require(&args.target_model, "--target-model")?;
    let Some(source_dim) = args.source_dim else {
        return Err(CliError::usage(
            "a converter needs --source-dim (the source space's dimension); --dim is the \
             target's and the probe can measure it",
        ));
    };
    let public_name = args
        .converter_name
        .clone()
        .unwrap_or_else(|| format!("univec-convert-{source_model}-to-{target_model}"));
    catalog::validate_public_name(&public_name)
        .map_err(|e| CliError::usage(format!("--converter-name: {e}")))?;
    Ok(Some(NewModel {
        placeholder_dim: providers::config::placeholder_dim("univec", &provider_target_id),
        id: provider_target_id,
        public_name,
        dim: args.dim,
        max_tokens: None,
        max_batch: None,
        catalogued: false,
        convert: Some(ConvertSpec {
            provider_source_id,
            source_model,
            target_model,
            source_dim,
        }),
    }))
}

/// The route id this converter will have — the same derivation the loader,
/// the serving descriptor and `provider rm` use, so "already declared with
/// this exact route" is decided by identity, not by field-by-field guesses.
fn converter_route_id(model: &NewModel) -> String {
    let convert = model.convert.as_ref().expect("a converter NewModel");
    providers::config::route_model_id(
        providers::config::ModelKind::Convert,
        &model.id,
        Some(&convert.provider_source_id),
        Some(&convert.source_model),
        Some(convert.source_dim),
        Some(&convert.target_model),
    )
}

/// The loader-schema view of a new converter entry, for the shared convert
/// probe. `None` for embed models. The dimension is the declared one when
/// known, else the placeholder — the probe uses it only to size its response
/// budget; declared-vs-measured is the caller's comparison.
fn probe_descriptor(model: &NewModel) -> Option<providers::config::ModelDescriptor> {
    let convert = model.convert.as_ref()?;
    Some(providers::config::ModelDescriptor {
        name: model.public_name.clone(),
        provider_model_id: model.id.clone(),
        dim: model.dim.unwrap_or(model.placeholder_dim),
        max_batch: providers::config::DEFAULT_MAX_BATCH,
        max_tokens: None,
        kind: providers::config::ModelKind::Convert,
        provider_source_id: Some(convert.provider_source_id.clone()),
        source_model: Some(convert.source_model.clone()),
        target_model: Some(convert.target_model.clone()),
        source_dim: Some(convert.source_dim),
    })
}

/// One `[[models]]` entry. Built in one place because it is appended in two:
/// before the probe for models whose dimension is already known, and after it
/// for the ones the probe measured.
fn model_entry(model: &NewModel) -> toml::Value {
    let mut entry = toml::map::Map::new();
    entry.insert(
        "name".into(),
        toml::Value::String(model.public_name.clone()),
    );
    entry.insert(
        "provider_model_id".into(),
        toml::Value::String(model.id.clone()),
    );
    // A placeholder until the probe measures one. It is replaced before the
    // file is written, and the write validates the real value.
    entry.insert(
        "dim".into(),
        toml::Value::Integer(model.dim.unwrap_or(model.placeholder_dim) as i64),
    );
    if let Some(max_batch) = model.max_batch {
        entry.insert("max_batch".into(), toml::Value::Integer(max_batch as i64));
    }
    if let Some(max_tokens) = model.max_tokens {
        entry.insert("max_tokens".into(), toml::Value::Integer(max_tokens as i64));
    }
    if let Some(convert) = &model.convert {
        entry.insert("kind".into(), toml::Value::String("convert".into()));
        entry.insert(
            "provider_source_id".into(),
            toml::Value::String(convert.provider_source_id.clone()),
        );
        entry.insert(
            "source_model".into(),
            toml::Value::String(convert.source_model.clone()),
        );
        entry.insert(
            "target_model".into(),
            toml::Value::String(convert.target_model.clone()),
        );
        entry.insert(
            "source_dim".into(),
            toml::Value::Integer(convert.source_dim as i64),
        );
    }
    toml::Value::Table(entry)
}

/// Models that get a live verification call.
///
/// Always the ones being added. If the connector itself changed (key or
/// endpoint), every model the file already declares too.
fn probe_targets(
    new_models: &[NewModel],
    already_declared: &[(String, String)],
    existing: Option<&ProviderFileDoc>,
    connector_changed: bool,
) -> Vec<(String, String, Option<u32>)> {
    let mut targets: Vec<(String, String, Option<u32>)> = new_models
        .iter()
        // Converter entries have their own probe; an embed call cannot
        // verify them and would be billed for the wrong thing. Catalogued
        // embeds are covered by the one-per-kind probe.
        .filter(|m| m.convert.is_none() && !m.catalogued)
        .map(|m| (m.id.clone(), m.public_name.clone(), m.dim))
        .collect();
    if !connector_changed {
        return targets;
    }
    let declared_dim = |name: &str| -> Option<u32> {
        existing?
            .value
            .get("models")?
            .as_array()?
            .iter()
            .find(|m| m.get("name").and_then(toml::Value::as_str) == Some(name))?
            .get("dim")?
            .as_integer()
            .and_then(|d| u32::try_from(d).ok())
    };
    for (public_name, id) in already_declared {
        targets.push((id.clone(), public_name.clone(), declared_dim(public_name)));
    }
    targets
}

/// Which key source this run records. No flag + an existing keyed file keeps
/// the existing source; no flag + a TTY prompts hidden; no flag otherwise is
/// a usage error naming the three sources.
fn resolve_key_spec(
    args: &ProviderAddArgs,
    canonical: &str,
    stem: &str,
    providers_dir: &std::path::Path,
    file_exists: bool,
    output: &Output,
) -> Result<KeySpec> {
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
    if file_exists && !args.api_key_from_login {
        return Ok(KeySpec::Existing);
    }
    if canonical == "univec" {
        if let Some(spec) = univec::login_key_offer(args, stem, providers_dir, output)? {
            return Ok(spec);
        }
    }
    if crate::proc::is_stdin_tty() && !output.is_json() {
        let raw = crate::proc::read_hidden_line("Provider API key (hidden): ")
            .map_err(|e| CliError::apply(format!("cannot read the key: {e}")))?;
        return non_empty_key(raw);
    }
    Err(CliError::usage(format!(
        "no key source: pass --api-key-file FILE (recommended), --api-key-env VAR, or \
         --key-stdin{} — a key is never accepted as a command-line value",
        if canonical == "univec" {
            ", or --api-key-from-login"
        } else {
            ""
        }
    )))
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
        KeySpec::Login { path, .. } => {
            clear(table);
            table.insert(file.into(), toml::Value::String(path.display().to_string()));
        }
        KeySpec::Existing => {}
    }
}

/// The secret value the verification probe uses. `--api-key-env` resolves
/// from *this* shell (the natural place a verifying operator has it);
/// `Existing` re-resolves whatever the file records.
fn probe_secret(key: &KeySpec, existing: Option<&ProviderFileDoc>) -> Result<String> {
    match key {
        KeySpec::Inline(value) | KeySpec::Login { key: value, .. } => Ok(value.clone()),
        KeySpec::File(path) => read_secret_file(path),
        KeySpec::Env(var) => std::env::var(var).map_err(|_| {
            CliError::precondition(format!(
                "--api-key-env {var} is not set in this shell; the key cannot be verified"
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
