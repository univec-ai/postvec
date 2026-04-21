//! `postvec model ls`.
//!
//! Installed `ls` is offline: it reconciles descriptors on disk,
//! ownership and (embedded targets) the engine's loaded set, but it
//! never fetches an index. Only `--available` talks to the registry.

use crate::cli::{Cli, ModelLsArgs};
use crate::commands::model::{
    admin, fetch_channel_index, human_bytes, resolve_target, ModelTarget,
};
use crate::error::{Exit, Result};
use crate::output::{Align, Cell, Column, Output, Tone};
use serde::Serialize;

pub async fn run(cli: &Cli, args: ModelLsArgs, output: &Output) -> Result<Exit> {
    if args.available {
        return run_available(cli, &args, output).await;
    }
    run_installed(cli, &args, output).await
}

#[derive(Serialize)]
struct AvailableDocument {
    schema_version: u32,
    command: &'static str,
    channel: String,
    authenticated: bool,
    models: Vec<AvailableRow>,
}

#[derive(Serialize)]
struct AvailableRow {
    name: String,
    access: String,
    model_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_dim: Option<u32>,
    download_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    /// `None` when no engine root was resolvable, so installation state is
    /// unknown, never collapsed to `false`. The human column prints `?`
    /// for the same case.
    #[serde(skip_serializing_if = "Option::is_none")]
    installed: Option<bool>,
    /// The registry's head revision for this name.
    revision: u64,
    /// The installed copy's revision, when one is installed with a receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_revision: Option<u64>,
    /// `not-installed`; `current` when the installed revision is at or past
    /// the head; `upgradable` when `postvec model upgrade` would act;
    /// `unknown` when the installed copy records no revision (a package or
    /// manual directory), so no comparison is possible. Absent when no engine
    /// root was resolvable at all. Unknown is never collapsed to `current`.
    #[serde(skip_serializing_if = "Option::is_none")]
    update: Option<&'static str>,
    withdrawn: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
}

/// What the installed side of the root knows about one name. `revision` is
/// `None` for a directory with no CLI receipt (package-owned or manual), which
/// means "not comparable", not "old".
struct InstalledState {
    revision: Option<u64>,
}

async fn run_available(cli: &Cli, args: &ModelLsArgs, output: &Output) -> Result<Exit> {
    let (index, credential) =
        fetch_channel_index(cli.timeout, args.api_key_file.as_deref(), output).await?;

    // Install markers are best-effort: with no resolvable root (no cluster,
    // no --path) the column simply reports unknown as "?".
    let installed: Option<std::collections::BTreeMap<String, InstalledState>> =
        match resolve_target(cli, None, output).await {
            Ok(target) => target.root().and_then(|root| {
                root.installed().ok().map(|models| {
                    models
                        .into_iter()
                        .map(|m| {
                            (
                                m.dir_name,
                                InstalledState {
                                    revision: m.receipt.as_ref().map(|r| r.revision()),
                                },
                            )
                        })
                        .collect()
                })
            }),
            Err(_) => None,
        };

    let rows: Vec<AvailableRow> = index
        .models
        .iter()
        .map(|m| {
            let state = installed.as_ref().map(|map| map.get(&m.name));
            AvailableRow {
                name: m.name.clone(),
                access: m.access.clone(),
                model_type: m.model_type.clone(),
                target_dim: m.target_dim,
                download_bytes: m.archive.size,
                license: m.license.clone(),
                installed: state.map(|state| state.is_some()),
                revision: m.revision(),
                installed_revision: state.flatten().and_then(|state| state.revision),
                update: state.map(|found| match found {
                    None => "not-installed",
                    // Installed without a receipt revision: a package or
                    // manual directory the CLI cannot compare, whatever the
                    // head happens to be.
                    Some(InstalledState { revision: None }) => "unknown",
                    Some(InstalledState {
                        revision: Some(installed),
                    }) if *installed >= m.revision() => "current",
                    Some(_) => "upgradable",
                }),
                withdrawn: m.withdrawn,
                summary: m.summary.clone(),
            }
        })
        .collect();

    let signed = credential
        .as_ref()
        .map(|c| format!(" (signed in as {})", c.masked()))
        .unwrap_or_default();
    let noun = if rows.len() == 1 { "model" } else { "models" };
    let mut human = format!(
        "{} catalogue — {} {noun}{signed}\n\n",
        index.channel,
        rows.len()
    );
    let columns = available_columns();
    let table_rows: Vec<Vec<Cell>> = rows
        .iter()
        .map(|row| {
            let (state, state_tone) = if row.withdrawn {
                ("withdrawn", Tone::Fail)
            } else {
                match row.installed {
                    Some(true) => ("installed", Tone::Success),
                    Some(false) => ("-", Tone::Dim),
                    None => ("?", Tone::Dim),
                }
            };
            // `?` is unknown — no resolvable root, or an install the CLI cannot
            // compare — and is never collapsed to "current".
            let (update, update_tone) = match (row.update, row.installed_revision) {
                (None, _) | (Some("unknown"), _) => ("?".to_string(), Tone::Dim),
                (Some("upgradable"), Some(from)) => {
                    (format!("{from}→{}", row.revision), Tone::Warn)
                }
                (Some(_), _) => ("-".to_string(), Tone::Dim),
            };
            vec![
                Cell::new(&row.name),
                Cell::with_tone(&row.model_type, Tone::Dim),
                Cell::new(
                    row.target_dim
                        .map(|d| d.to_string())
                        .unwrap_or_else(|| "-".into()),
                ),
                Cell::new(human_bytes(row.download_bytes)),
                Cell::with_tone(&row.access, Tone::Dim),
                Cell::new(row.license.as_deref().unwrap_or("-")),
                Cell::new(row.revision.to_string()),
                Cell::with_tone(update, update_tone),
                Cell::with_tone(state, state_tone),
            ]
        })
        .collect();
    if !table_rows.is_empty() {
        human.push_str(&output.render_table(&columns, &table_rows));
    }
    if rows.iter().any(|r| r.update == Some("upgradable")) {
        human.push_str(&format!(
            "\n{}\n",
            output
                .style
                .warn("run `postvec model upgrade <name>` (or --all) to replace them in place")
        ));
    }

    output.show_document(
        &AvailableDocument {
            schema_version: crate::checks::SCHEMA_VERSION,
            command: "model ls",
            channel: index.channel.clone(),
            authenticated: index.authenticated,
            models: rows,
        },
        &human,
    )?;
    Ok(Exit::Success)
}

#[derive(Serialize)]
struct InstalledDocument {
    schema_version: u32,
    command: &'static str,
    target: String,
    models: Vec<InstalledRow>,
}

#[derive(Serialize)]
struct InstalledRow {
    name: String,
    backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_dim: Option<u32>,
    disk_bytes: u64,
    enabled: bool,
    owner: String,
    /// From the receipt; absent for package-owned and manual directories.
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loaded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    receipt_error: Option<String>,
}

async fn run_installed(cli: &Cli, args: &ModelLsArgs, output: &Output) -> Result<Exit> {
    let target = resolve_target(cli, args.path.as_deref(), output).await?;

    // Remote mode: list what the configured nodes advertise.
    if let ModelTarget::Remote { settings, .. } = &target {
        let probe = crate::commands::collect::probe_remote(
            settings,
            crate::cli::TlsPolicy::ExtensionCompatible,
            cli.timeout,
        )
        .await?;
        let names = probe.advertised_enabled();
        let noun = if names.len() == 1 { "model" } else { "models" };
        let mut human = format!(
            "remote mode — {} {noun} advertised by the configured inference nodes\n\n",
            names.len()
        );
        for name in &names {
            human.push_str(&format!("  {name}\n"));
        }
        human.push_str(&format!(
            "\n{}\n",
            output
                .style
                .dim("(use --path <DIR> to inspect a local engine root)")
        ));
        #[derive(Serialize)]
        struct RemoteDocument {
            schema_version: u32,
            command: &'static str,
            target: String,
            advertised: Vec<String>,
        }
        output.show_document(
            &RemoteDocument {
                schema_version: crate::checks::SCHEMA_VERSION,
                command: "model ls",
                target: target.label(),
                advertised: names.into_iter().collect(),
            },
            &human,
        )?;
        return Ok(Exit::Success);
    }

    let root = target.root().expect("path or embedded target has a root");
    let _shared = root.lock_shared()?;
    let inventory = root.installed()?;

    // Loaded state is knowable only where a launcher listener exists.
    let loaded = match &target {
        ModelTarget::Embedded { settings, .. } => {
            admin::loaded_inventory(&settings.embedded_http_listen(), cli.timeout)
                .await
                .map(|inv| inv.enabled_names())
        }
        _ => None,
    };

    let mut rows = Vec::new();
    for model in &inventory {
        let owner = model.ownership(cli.timeout).await;
        rows.push(InstalledRow {
            loaded: loaded.as_ref().map(|set| {
                set.contains(model.descriptor_name.as_deref().unwrap_or(&model.dir_name))
            }),
            name: model.dir_name.clone(),
            backend: model.backend.clone(),
            model_type: model.model_type.clone(),
            target_dim: model.target_dim,
            disk_bytes: model.disk_bytes,
            enabled: model.enabled,
            revision: model.receipt.as_ref().map(|r| r.revision()),
            owner: match owner {
                crate::registry::root::Ownership::Cli => "CLI",
                crate::registry::root::Ownership::Package => "package",
                crate::registry::root::Ownership::Manual => "manual",
            }
            .to_string(),
            receipt_error: model.receipt_error.clone(),
        });
    }

    let mut human = String::new();
    if rows.is_empty() {
        human.push_str(&format!(
            "no models installed under {}\n",
            root.models_dir().display()
        ));
    } else {
        let columns = installed_columns();
        let table_rows: Vec<Vec<Cell>> = rows
            .iter()
            .map(|row| {
                // `enabled` is the persistent power switch; `loaded` is what
                // the engine holds right now. The two disagreeing is worth
                // saying out loud in both directions: "not loaded" while
                // enabled means something failed, and "loaded" while
                // deactivated means an unload did not finish.
                let (state, tone) = match (row.loaded, row.enabled) {
                    (Some(true), true) => (format!("loaded ({})", row.owner), Tone::Success),
                    (Some(true), false) => (
                        format!("loaded, deactivate pending ({})", row.owner),
                        Tone::Fail,
                    ),
                    (Some(false), false) => (format!("deactivated ({})", row.owner), Tone::Dim),
                    (Some(false), true) => (format!("not loaded ({})", row.owner), Tone::Warn),
                    (None, false) => (format!("deactivated ({})", row.owner), Tone::Dim),
                    (None, true) => (format!("on disk ({})", row.owner), Tone::Dim),
                };
                vec![
                    Cell::new(&row.name),
                    Cell::with_tone(row.model_type.as_deref().unwrap_or("-"), Tone::Dim),
                    Cell::new(
                        row.target_dim
                            .map(|d| d.to_string())
                            .unwrap_or_else(|| "-".into()),
                    ),
                    Cell::new(human_bytes(row.disk_bytes)),
                    Cell::new(
                        row.revision
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| "-".into()),
                    ),
                    Cell::with_tone(state, tone),
                ]
            })
            .collect();
        human.push_str(&output.render_table(&columns, &table_rows));
        for row in &rows {
            if let Some(error) = &row.receipt_error {
                human.push_str(&format!("  {} {}\n", output.style.fail("receipt:"), error));
            }
        }
    }

    output.show_document(
        &InstalledDocument {
            schema_version: crate::checks::SCHEMA_VERSION,
            command: "model ls",
            target: target.label(),
            models: rows,
        },
        &human,
    )?;
    Ok(Exit::Success)
}

fn available_columns() -> [Column; 9] {
    [
        Column {
            header: "NAME",
            align: Align::Left,
            min: 12,
            max: 44,
            shrink: 4,
        },
        Column {
            header: "TYPE",
            align: Align::Left,
            min: 4,
            max: 8,
            shrink: 1,
        },
        Column {
            header: "DIM",
            align: Align::Right,
            min: 3,
            max: 5,
            shrink: 0,
        },
        Column {
            header: "SIZE",
            align: Align::Right,
            min: 5,
            max: 9,
            shrink: 0,
        },
        Column {
            header: "ACCESS",
            align: Align::Left,
            min: 6,
            max: 7,
            shrink: 2,
        },
        Column {
            header: "LICENSE",
            align: Align::Left,
            min: 6,
            max: 12,
            shrink: 2,
        },
        Column {
            header: "REV",
            align: Align::Right,
            min: 3,
            max: 4,
            shrink: 0,
        },
        Column {
            header: "UPDATE",
            align: Align::Left,
            min: 1,
            max: 8,
            shrink: 1,
        },
        Column {
            header: "STATE",
            align: Align::Left,
            min: 1,
            max: 10,
            shrink: 1,
        },
    ]
}

fn installed_columns() -> [Column; 6] {
    [
        Column {
            header: "NAME",
            align: Align::Left,
            min: 12,
            max: 44,
            shrink: 2,
        },
        Column {
            header: "TYPE",
            align: Align::Left,
            min: 4,
            max: 8,
            shrink: 1,
        },
        Column {
            header: "DIM",
            align: Align::Right,
            min: 3,
            max: 5,
            shrink: 0,
        },
        Column {
            header: "SIZE",
            align: Align::Right,
            min: 5,
            max: 9,
            shrink: 0,
        },
        Column {
            header: "REV",
            align: Align::Right,
            min: 3,
            max: 4,
            shrink: 0,
        },
        Column {
            header: "STATE",
            align: Align::Left,
            min: 8,
            max: 22,
            shrink: 1,
        },
    ]
}
