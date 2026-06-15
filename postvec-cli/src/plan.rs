//! Plans: what a mutating command intends to do, shown before it does it.
//!
//! Every mutation a command can perform appears as a [`PlanStep`], and the plan
//! is rendered before the confirmation prompt. `--dry-run` stops after that
//! rendering, which is what makes "dry run changes nothing" a structural
//! property rather than a promise.
//!
//! The [`ApplyJournal`] is the other half: it records what actually happened,
//! so a partial multi-database result can name exactly which targets changed.

use crate::cli::Mode;
use crate::error::{CliError, Exit, Result};
use crate::proc;
use serde::Serialize;
use std::io::BufRead;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PlanStep {
    CreateDatabase {
        name: String,
    },
    CreateExtension {
        database: String,
    },
    WriteConfig {
        path: PathBuf,
        #[serde(skip_serializing_if = "Option::is_none")]
        before_sha256: Option<String>,
        after_sha256: String,
    },
    /// `provider add`: the exact billed verification this run will make,
    /// stated before confirmation.
    VerifyProviders {
        provider: String,
        embed_probes: usize,
        convert_probes: usize,
    },
    RemoveConfig {
        path: PathBuf,
    },
    RestartCluster {
        unit: String,
    },
    /// `uninstall --purge`: the cluster is stopped for the file sweep and
    /// started again afterwards.
    StopCluster {
        unit: String,
    },
    /// Only SIGHUP-context settings changed, so the worker picks them up
    /// without an outage.
    ReloadConfig {
        database: String,
    },
    SmokeCheck {
        database: String,
    },
    CleanupPostvecObjects {
        database: String,
        drop_columns: bool,
        /// Whether recursive entries' managed chunk destinations go too.
        /// Only meaningful with `drop_columns`.
        #[serde(default)]
        drop_destinations: bool,
    },
    DropExtension {
        database: String,
    },
    /// `uninstall --purge`: delete one path on this host that postvec (the
    /// CLI, `setup`, a model pull, or a manual install) created.
    RemovePath {
        path: PathBuf,
        /// What the path is, for the operator ("pulled model baai-bge-m3").
        what: String,
    },
    /// Model management (`postvec model …`).
    DownloadArchive {
        name: String,
        bytes: u64,
        /// Why the entry is in the closure.
        reason: &'static str,
    },
    InstallModel {
        name: String,
        backend: String,
    },
    /// Replace an installed model with a newer revision of the same name,
    /// in place.
    UpgradeModel {
        name: String,
        backend: String,
        from_revision: u64,
        to_revision: u64,
        /// An embedding model, whose weights *are* its vector space, so its
        /// outputs may differ from the installed revision's.
        embed: bool,
    },
    RemoveModel {
        name: String,
        path: PathBuf,
    },
    LoadModels {
        models: Vec<String>,
    },
    UnloadModel {
        name: String,
    },
    /// Flip the installed descriptor's `enabled` field, which is what makes
    /// activation and deactivation survive a PostgreSQL restart.
    SetModelEnabled {
        name: String,
        enabled: bool,
        /// True when this model is only being enabled because something else
        /// being activated depends on it.
        dependency_of: Option<String>,
    },
    /// Not a mutation: the loud, per-model warning that managed columns lose
    /// their embedding route to a model this command is about to take away.
    /// Carrying it as a step is what lets [`Plan::in_use_models`] gate the
    /// confirmation on it.
    AcknowledgeInUse {
        model: String,
        columns: Vec<InUseColumn>,
        /// Databases whose registry could not be read, so "not in use" was
        /// never established for them.
        unknown_databases: Vec<String>,
    },
    /// Not a mutation: the inverse of [`PlanStep::AcknowledgeInUse`] —
    /// existing columns bound to this public name START sending their source
    /// text to an external provider once it serves the name (the
    /// bridge-upgrade privacy event). Carried as a
    /// step so [`Plan::in_use_models`] gates the confirmation on it.
    AcknowledgeProviderPrivacy {
        provider: String,
        model: String,
        columns: Vec<InUseColumn>,
        /// Databases whose registry could not be read, so "no column is
        /// affected" was never established for them.
        unknown_databases: Vec<String>,
    },
    RefreshModelCache {
        database: String,
    },
}

/// One managed column that loses its embedding route, as `postvec.registry`
/// records it.
#[derive(Debug, Clone, Serialize)]
pub struct InUseColumn {
    /// Which model going away takes this column's route with it — one scan can
    /// cover several at once.
    pub model: String,
    /// The space the column actually declares. Often *not* `model`: a column
    /// on a convert-only target is served by a converter and the
    /// `embed-bridge` executor, neither of which carries the target's name.
    pub declared_model: String,
    pub database: String,
    /// `schema.table`.
    pub relation: String,
    pub column: String,
    /// The registry entry's state (`active`, `migrating`, `disabled`).
    pub state: String,
}

impl InUseColumn {
    pub fn describe(&self) -> String {
        let route = if self.declared_model == self.model {
            String::new()
        } else {
            // Say why a column that never names this model is affected, or the
            // warning reads as a false positive.
            format!(" — declared on {}, served through it", self.declared_model)
        };
        format!(
            "{}: {}.{} ({}){route}",
            self.database, self.relation, self.column, self.state
        )
    }
}

impl PlanStep {
    /// One line describing the step, for the plan and the journal.
    pub fn describe(&self) -> String {
        match self {
            PlanStep::CreateDatabase { name } => format!("create database {name:?}"),
            PlanStep::CreateExtension { database } => {
                format!("install the postvec extension in {database:?}")
            }
            PlanStep::VerifyProviders {
                provider,
                embed_probes,
                convert_probes,
            } => format!(
                "verify against {provider} with {embed_probes} billed embed probe(s) and \
                 {convert_probes} billed convert probe(s)"
            ),
            PlanStep::WriteConfig {
                path,
                before_sha256,
                ..
            } => format!(
                "{} {}",
                if before_sha256.is_some() {
                    "rewrite"
                } else {
                    "create"
                },
                path.display()
            ),
            PlanStep::RemoveConfig { path } => format!("remove {}", path.display()),
            PlanStep::RestartCluster { unit } => format!("restart {unit}"),
            PlanStep::StopCluster { unit } => {
                format!("stop {unit} for the file sweep, then start it again")
            }
            PlanStep::ReloadConfig { database } => format!(
                "reload the configuration (via {database:?}) so the worker picks up the new \
                 endpoints; no restart needed"
            ),
            PlanStep::SmokeCheck { database } => {
                format!("verify the worker and inference path for {database:?}")
            }
            PlanStep::CleanupPostvecObjects {
                database,
                drop_columns,
                drop_destinations,
            } => format!(
                "remove postvec's runtime objects in {database:?}{}",
                match (*drop_columns, *drop_destinations) {
                    (true, true) => " AND DROP THE SHADOW VECTOR COLUMNS AND CHUNK DESTINATIONS",
                    (true, false) =>
                        " AND DROP THE SHADOW VECTOR COLUMNS (keeping chunk destinations)",
                    (false, _) => " (keeping the shadow vector columns and chunk destinations)",
                }
            ),
            PlanStep::RemovePath { path, what } => format!("delete {} ({what})", path.display()),
            PlanStep::DropExtension { database } => {
                format!("drop the postvec extension from {database:?} (without CASCADE)")
            }
            PlanStep::DownloadArchive {
                name,
                bytes,
                reason,
            } => format!(
                "download and verify {name} ({}) — {reason}",
                crate::commands::model::human_bytes(*bytes)
            ),
            PlanStep::InstallModel { name, backend } => format!(
                "install {name} under models/{backend}/, deactivated — run `postvec model \
                 activate {name}` to serve it"
            ),
            PlanStep::UpgradeModel {
                name,
                backend,
                from_revision,
                to_revision,
                embed,
            } => format!(
                "replace models/{backend}/{name} in place: revision {from_revision} → \
                 {to_revision}{}. Existing stored vectors are NOT rewritten by this upgrade",
                if *embed {
                    ", whose outputs may differ from the installed revision's"
                } else {
                    ""
                }
            ),
            PlanStep::RemoveModel { name, path } => {
                format!("remove {name} ({})", path.display())
            }
            PlanStep::LoadModels { models } => format!(
                "load into the running embedded engine: {}",
                models.join(", ")
            ),
            PlanStep::UnloadModel { name } => {
                format!("unload {name} from the running embedded engine")
            }
            PlanStep::SetModelEnabled {
                name,
                enabled,
                dependency_of,
            } => format!(
                "mark {name} {} on disk (survives a restart){}",
                if *enabled { "enabled" } else { "disabled" },
                match dependency_of {
                    Some(root) => format!(" — {root} depends on it"),
                    None => String::new(),
                }
            ),
            PlanStep::AcknowledgeInUse {
                model,
                columns,
                unknown_databases,
            } => {
                let mut text =
                    format!("WARNING: {model} is the embedding route for these columns:");
                for column in columns {
                    text.push_str(&format!("\n      {}", column.describe()));
                }
                for database in unknown_databases {
                    text.push_str(&format!(
                        "\n      {database}: could not be inspected, so columns depending on \
                         {model} are UNKNOWN"
                    ));
                }
                text.push_str(&format!(
                    "\n    search(), embed() and the worker will fail for those entries until \
                     {model} is activated again, or the column is migrated to another model or \
                     disabled. Stored vectors are not touched."
                ));
                text
            }
            PlanStep::AcknowledgeProviderPrivacy {
                provider,
                model,
                columns,
                unknown_databases,
            } => {
                let mut text = format!(
                    "WARNING: these columns are bound to {model}, which external provider \
                     {provider:?} is about to serve:"
                );
                for column in columns {
                    text.push_str(&format!("\n      {}", column.describe()));
                }
                for database in unknown_databases {
                    text.push_str(&format!(
                        "\n      {database}: could not be inspected, so columns bound to \
                         {model} are UNKNOWN"
                    ));
                }
                text.push_str(&format!(
                    "\n    from the next worker cycle their SOURCE TEXT is sent to \
                     {provider:?} for embedding — no SQL change and no further notice. \
                     Stored vectors are not touched."
                ));
                text
            }
            PlanStep::RefreshModelCache { database } => {
                format!("refresh postvec.models in {database:?}")
            }
        }
    }

    /// Whether this step destroys data that cannot be recovered. Removing a
    /// pulled model is deliberately not in this set: the registry's immutable
    /// names make a re-pull byte-identical.
    pub fn is_destructive(&self) -> bool {
        matches!(
            self,
            PlanStep::CleanupPostvecObjects {
                drop_columns: true,
                ..
            }
        )
    }
}

/// One distinct terms document covering models a plan downloads or replaces,
/// grouped by the exact (license, version, URL, acceptance) tuple.
/// Rendered in the plan's "terms in this plan" block and carried verbatim in
/// the JSON result, never hidden in stderr-only notes.
#[derive(Debug, Clone, Serialize)]
pub struct TermsDocument {
    pub license: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The canonical HTTPS document URL — never an archive source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `"none"` | `"notice"` (an `"organization"` entry is refused before a
    /// plan exists).
    pub acceptance: String,
    /// Sorted names of every affected model — requested, dependencies and
    /// postvec companions alike.
    pub models: Vec<String>,
    /// Whether this run must obtain a local acknowledgement: a notice
    /// policy not already evidenced by every installation being replaced.
    pub acknowledgement_required: bool,
    /// The exact ready-to-paste flag, when acknowledgement is required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accept_flag: Option<String>,
}

impl TermsDocument {
    /// The `<id>@<version>` half of the non-interactive flag, for versioned
    /// documents.
    pub fn token(&self) -> Option<String> {
        self.version
            .as_ref()
            .map(|version| format!("{}@{version}", self.license))
    }

    /// The plan lines for this document.
    pub fn describe(&self) -> Vec<String> {
        let mut heading = self.license.clone();
        if let Some(version) = &self.version {
            heading.push(' ');
            heading.push_str(version);
        }
        if self.acceptance != "none" {
            heading.push_str(&format!(" ({})", self.acceptance));
        }
        heading.push_str(&format!(" — {}", self.models.join(", ")));
        let mut lines = vec![heading];
        if let Some(url) = &self.url {
            lines.push(format!("    {url}"));
        }
        if let Some(flag) = &self.accept_flag {
            lines.push(format!("    requires acknowledgement: {flag}"));
        } else if self.acceptance == "notice" {
            lines.push("    already acknowledged for the installed copy".to_string());
        }
        lines
    }
}

/// A database this command will act on, and what is already true of it.
#[derive(Debug, Clone, Serialize)]
pub struct DatabasePlan {
    pub name: String,
    pub exists: bool,
    /// Whether the extension is already installed.
    pub extension_installed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    pub command: &'static str,
    pub cluster: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    pub databases: Vec<DatabasePlan>,
    /// The distinct terms documents this plan's downloads/replacements are
    /// under. Empty (and omitted from JSON) for every command without one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub terms: Vec<TermsDocument>,
    pub steps: Vec<PlanStep>,
    pub restart_required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_label: Option<String>,
    pub destructive: bool,
    /// Human-readable description of the inference target, for the prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference: Option<String>,
    /// Where the owned configuration lives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_path: Option<PathBuf>,
}

impl Plan {
    pub fn new(command: &'static str, cluster: impl Into<String>) -> Self {
        Self {
            command,
            cluster: cluster.into(),
            mode: None,
            databases: Vec::new(),
            terms: Vec::new(),
            steps: Vec::new(),
            restart_required: false,
            restart_label: None,
            destructive: false,
            inference: None,
            config_path: None,
        }
    }

    pub fn push(&mut self, step: PlanStep) {
        if step.is_destructive() {
            self.destructive = true;
        }
        if let PlanStep::RestartCluster { unit } = &step {
            self.restart_required = true;
            self.restart_label = Some(unit.clone());
        }
        self.steps.push(step);
    }

    /// True when the plan would change nothing.
    ///
    /// An acknowledgement step is not a mutation, so a plan carrying only
    /// those is still a no-op — and must not be able to demand confirmation
    /// for work it is not doing.
    pub fn is_noop(&self) -> bool {
        self.steps.iter().all(|step| {
            matches!(
                step,
                PlanStep::AcknowledgeInUse { .. }
                    | PlanStep::AcknowledgeProviderPrivacy { .. }
                    | PlanStep::VerifyProviders { .. }
            )
        })
    }

    /// The models this plan takes an embedding route away from, in plan order.
    /// Non-empty means the ordinary confirmation is not enough: interactively
    /// the operator types the names, non-interactively they pass
    /// `--acknowledge-in-use` alongside `--yes`.
    ///
    /// An `--acknowledge-in-use` passed when this is empty is **reported, not
    /// refused** — the callers note it. The `--accept-license` precedent
    /// refuses a stale token because that flag names a *specific* document, so
    /// "always pass it" is impossible; this one is a bare boolean, and
    /// refusing it would break the reasonable habit of passing it
    /// unconditionally in automation, on exactly the runs where nothing was
    /// wrong.
    pub fn in_use_models(&self) -> Vec<String> {
        self.steps
            .iter()
            .filter_map(|step| match step {
                PlanStep::AcknowledgeInUse { model, .. }
                | PlanStep::AcknowledgeProviderPrivacy { model, .. } => Some(model.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn headline(&self) -> String {
        if self.is_noop() {
            format!(
                "postvec {}: cluster {} is already in the requested state",
                self.command, self.cluster
            )
        } else {
            format!(
                "postvec {} will change cluster {}:",
                self.command, self.cluster
            )
        }
    }

    /// The lines shown before the confirmation prompt. Everything the operator
    /// needs to decide: cluster, config file, databases, inference target,
    /// restart.
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(path) = &self.config_path {
            lines.push(format!("configuration file: {}", path.display()));
        }
        if let Some(mode) = self.mode {
            lines.push(match &self.inference {
                Some(inference) => format!("inference: {mode} ({inference})"),
                None => format!("inference: {mode}"),
            });
        }
        if !self.databases.is_empty() {
            lines.push(format!(
                "databases: {}",
                self.databases
                    .iter()
                    .map(|database| {
                        let mut label = database.name.clone();
                        if !database.exists {
                            label.push_str(" (to be created)");
                        } else if database.extension_installed {
                            label.push_str(" (extension already installed)");
                        }
                        label
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for step in &self.steps {
            lines.push(format!("- {}", step.describe()));
        }
        if !self.terms.is_empty() {
            lines.push("terms in this plan:".to_string());
            for document in &self.terms {
                for line in document.describe() {
                    lines.push(format!("  {line}"));
                }
            }
        }
        if lines.is_empty() {
            lines.push("nothing to do".to_string());
        }
        lines
    }
}

/// Whether a human can answer a question on this invocation.
///
/// An explicit capability rather than a call to `isatty` inside [`confirm`]:
/// with the probe buried in the decision, the behaviour of a confirmation
/// depends on how the *process* was started, which makes it impossible to test
/// both branches — and a test that happens to run with a terminal attached
/// blocks forever waiting for input nobody is there to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prompt {
    interactive: bool,
}

impl Prompt {
    /// The real invocation: interactive exactly when stdin is a terminal.
    pub fn from_environment() -> Self {
        Self {
            interactive: proc::is_stdin_tty(),
        }
    }

    /// A caller that cannot answer questions. Real invocations reach this
    /// state through [`Prompt::from_environment`]; it is constructible directly
    /// only in tests, so no test can depend on — or block on — the terminal it
    /// happens to run under.
    #[cfg(test)]
    pub fn non_interactive() -> Self {
        Self { interactive: false }
    }

    /// An interactive caller, constructible only in tests (the paired
    /// question-asking closure is injected there, so nothing reads a real
    /// terminal).
    #[cfg(test)]
    pub fn interactive() -> Self {
        Self { interactive: true }
    }

    /// Whether a human can be asked a question on this invocation.
    pub fn is_interactive(&self) -> bool {
        self.interactive
    }
}

/// Gate a plan that takes a model away from columns that depend on it for
/// their embedding route.
///
/// Deliberately separate from [`confirm`]'s `--yes`, and from `--force`:
///
/// - `--yes` answers "this is the change I meant"; it cannot also answer
///   "I accept that these columns stop working", which the operator may not
///   have known about when they wrote the command.
/// - `--force` answers a different question — "break other *models* that
///   depend on this name" — and a flag that meant both would let one
///   acknowledgement stand in for the other.
///
/// Interactively the operator types the model names; non-interactively they
/// pass `--acknowledge-in-use` **and** `--yes`. A dry run asks nothing: the
/// plan it just printed carries the warning and the exact flag.
///
/// Runs before [`confirm`], so a refusal here changes nothing.
pub fn confirm_in_use(
    plan: &Plan,
    acknowledged: bool,
    yes: bool,
    dry_run: bool,
    prompt: Prompt,
    ask: impl FnMut(&[String]) -> Result<String>,
) -> Result<()> {
    confirm_in_use_with(
        plan,
        acknowledged,
        yes,
        dry_run,
        prompt,
        ask,
        "managed columns lose their embedding route to {models}; pass --acknowledge-in-use \
         together with --yes to proceed knowing those entries will fail. --yes and --force \
         deliberately do not stand in for it",
    )
}

/// [`confirm_in_use`] with a caller-supplied consequence sentence for the
/// non-interactive refusal (`{models}` is substituted). The provider-privacy
/// gate shares the mechanics but not the wording: there the columns keep
/// working — their text starts leaving the host.
pub fn confirm_in_use_with(
    plan: &Plan,
    acknowledged: bool,
    yes: bool,
    dry_run: bool,
    prompt: Prompt,
    mut ask: impl FnMut(&[String]) -> Result<String>,
    consequence: &str,
) -> Result<()> {
    let models = plan.in_use_models();
    if models.is_empty() || dry_run {
        return Ok(());
    }
    if acknowledged {
        return Ok(());
    }
    if yes || !prompt.is_interactive() {
        return Err(CliError::usage(
            consequence.replace("{models}", &models.join(", ")),
        ));
    }
    let typed = ask(&models)?;
    if !in_use_answer_matches(&typed, &models) {
        return Err(CliError::usage(format!(
            "confirmation did not match; nothing was changed. Expected: {}",
            models.join(" ")
        )));
    }
    Ok(())
}

/// Does the typed answer name exactly the models the plan is taking away?
///
/// The prompt shows the names bare, so the obvious thing to type is what is on
/// screen — but an operator copying from the warning block may also paste
/// quotes, commas or stray spacing. The acknowledgement exists to prove the
/// operator read *which* models are affected, not to test their punctuation,
/// so separators and surrounding quotes are normalized away. Order is not:
/// the names are compared as a set.
fn in_use_answer_matches(typed: &str, models: &[String]) -> bool {
    let normalize = |raw: &str| -> std::collections::BTreeSet<String> {
        raw.split([',', ' ', '\t', '\n', '\r'])
            .map(|token| token.trim_matches(['"', '\'']).trim())
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .collect()
    };
    let expected: std::collections::BTreeSet<String> = models.iter().cloned().collect();
    !expected.is_empty() && normalize(typed) == expected
}

/// The interactive in-use acknowledgement: the plan has already printed which
/// columns are affected, so this only asks the operator to type the names back.
///
/// The names are printed **bare**. A `{:?}` rendering would put quotes on
/// screen that the obvious copy-paste then has to survive — the first thing an
/// operator does with this prompt is type what they can see.
pub fn interactive_in_use_acknowledgement(models: &[String]) -> Result<String> {
    eprint!(
        "Type {} to confirm those columns will stop working: ",
        models.join(" ")
    );
    read_line()
}

/// The interactive acknowledgement for a change that does *both* — some
/// columns lose their route, others are handed to a different recipient. The
/// two single-purpose prompts each describe only half of it, and a prompt
/// that names the wrong consequence is worse than a generic one.
pub fn interactive_route_change_acknowledgement(models: &[String]) -> Result<String> {
    eprint!(
        "Type {} to confirm those columns' embedding routes may change — lost for some, \
         sent to a different provider for others, as the plan lists: ",
        models.join(" ")
    );
    read_line()
}

/// The interactive provider-privacy acknowledgement: the plan has printed
/// which columns start sending text; the operator types the names back.
pub fn interactive_provider_privacy_acknowledgement(models: &[String]) -> Result<String> {
    eprint!(
        "Type {} to confirm those columns' source text may be sent to the provider: ",
        models.join(" ")
    );
    read_line()
}

/// Ask for confirmation, once, after the whole plan is known.
///
/// A non-interactive caller without `--yes` is a usage error *before* any
/// mutation: silently proceeding in a script would be the worst possible
/// default for a command that restarts a database server.
pub fn confirm(
    plan: &Plan,
    yes: bool,
    destructive_ack: Option<&str>,
    prompt: Prompt,
) -> Result<()> {
    if plan.is_noop() {
        return Ok(());
    }
    // A typed database-name acknowledgement is required for data loss, and
    // `--yes` alone is deliberately not enough for it.
    if let Some(expected) = destructive_ack {
        if !yes {
            if !prompt.interactive {
                return Err(CliError::usage(
                    "this command destroys data and needs confirmation; pass --yes together \
                     with --acknowledge-data-loss to run it non-interactively",
                ));
            }
            eprint!("Type the database name {expected:?} to confirm data loss: ");
            let typed = read_line()?;
            if typed.trim() != expected {
                return Err(CliError::usage(
                    "confirmation did not match; nothing was changed",
                ));
            }
        }
        return Ok(());
    }
    if yes {
        return Ok(());
    }
    if !prompt.interactive {
        return Err(CliError::usage(
            "refusing to change a cluster without confirmation; pass --yes for \
             non-interactive use, or --dry-run to see the plan",
        ));
    }
    eprint!("Proceed? [y/N] ");
    let answer = read_line()?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        Err(CliError::usage("cancelled; nothing was changed"))
    }
}

pub(crate) fn read_line() -> Result<String> {
    use std::io::Write;
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| CliError::usage(format!("cannot read confirmation: {e}")))?;
    Ok(line)
}

/// What actually happened. Kept in memory and included in the result, so a
/// partial outcome names exactly which targets changed.
#[derive(Debug, Default, Serialize)]
pub struct ApplyJournal {
    pub applied: Vec<String>,
    pub succeeded_databases: Vec<String>,
    pub failed_databases: Vec<FailedDatabase>,
    /// Work the command promised but could not complete, even though nothing
    /// failed outright — a configuration file it does not own, say. Reporting
    /// success here would tell the operator a job is finished when it is not.
    pub incomplete: Vec<String>,
    pub restart_deferred: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FailedDatabase {
    pub name: String,
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

impl ApplyJournal {
    pub fn record(&mut self, message: impl Into<String>) {
        self.applied.push(message.into());
    }

    pub fn succeeded(&mut self, database: impl Into<String>) {
        self.succeeded_databases.push(database.into());
    }

    /// Record something the command could not finish. Downgrades the outcome
    /// to a partial result without claiming a failure.
    pub fn incomplete(&mut self, reason: impl Into<String>) {
        self.incomplete.push(reason.into());
    }

    pub fn failed(&mut self, database: impl Into<String>, error: &CliError) {
        self.failed_databases.push(FailedDatabase {
            name: database.into(),
            error: error.to_string(),
            remediation: error.remediation().map(str::to_string),
        });
    }

    /// The exit code the journal implies.
    ///
    /// A deferred restart outranks a partial result: the operator's next action
    /// is the restart either way, and exit 4 is the code that says so.
    pub fn exit(&self) -> Exit {
        if !self.failed_databases.is_empty() {
            return if self.succeeded_databases.is_empty() {
                Exit::Failure
            } else {
                Exit::Partial
            };
        }
        if self.restart_deferred {
            return Exit::RestartRequired;
        }
        if !self.incomplete.is_empty() {
            return Exit::Partial;
        }
        Exit::Success
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_plan() -> Plan {
        let mut plan = Plan::new("setup", "18/main");
        plan.mode = Some(Mode::Grpc);
        plan.inference = Some("gRPC 192.0.2.2:33333, discovery https://192.0.2.2:22222".into());
        plan.config_path = Some(PathBuf::from(
            "/etc/postgresql/18/main/conf.d/99-postvec.conf",
        ));
        plan.databases = vec![DatabasePlan {
            name: "univec".into(),
            exists: false,
            extension_installed: false,
        }];
        plan.push(PlanStep::CreateDatabase {
            name: "univec".into(),
        });
        plan.push(PlanStep::CreateExtension {
            database: "univec".into(),
        });
        plan.push(PlanStep::WriteConfig {
            path: PathBuf::from("/etc/postgresql/18/main/conf.d/99-postvec.conf"),
            before_sha256: None,
            after_sha256: "abc".into(),
        });
        plan.push(PlanStep::RestartCluster {
            unit: "postgresql@18-main.service".into(),
        });
        plan.push(PlanStep::SmokeCheck {
            database: "univec".into(),
        });
        plan
    }

    #[test]
    fn a_restart_step_marks_the_plan_and_names_the_unit() {
        let plan = setup_plan();
        assert!(plan.restart_required);
        assert_eq!(
            plan.restart_label.as_deref(),
            Some("postgresql@18-main.service")
        );
    }

    #[test]
    fn the_plan_names_everything_the_operator_must_decide_on() {
        let described = setup_plan().describe().join("\n");
        assert!(described.contains("/etc/postgresql/18/main/conf.d/99-postvec.conf"));
        assert!(described.contains("inference: grpc"));
        assert!(described.contains("192.0.2.2:33333"));
        assert!(described.contains("univec (to be created)"));
        assert!(described.contains("create database"));
        assert!(described.contains("restart postgresql@18-main.service"));
    }

    #[test]
    fn every_mutation_appears_as_a_step() {
        let plan = setup_plan();
        let kinds: Vec<String> = plan
            .steps
            .iter()
            .map(|step| {
                serde_json::to_value(step).unwrap()["kind"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "create-database",
                "create-extension",
                "write-config",
                "restart-cluster",
                "smoke-check"
            ]
        );
    }

    /// Terms block: version, canonical URL, policy, sorted models and the
    /// ready-to-paste flag when acknowledgement is required. Same shape in
    /// JSON. Plans with no terms omit the field.
    #[test]
    fn the_terms_block_is_rendered_and_serialized() {
        let mut plan = Plan::new("model pull", "path:/opt/root");
        plan.terms = vec![
            TermsDocument {
                license: "apache-2.0".into(),
                version: None,
                url: None,
                acceptance: "none".into(),
                models: vec!["embed-a".into(), "embed-bridge".into()],
                acknowledgement_required: false,
                accept_flag: None,
            },
            TermsDocument {
                license: "univec-commercial".into(),
                version: Some("2026-08-09".into()),
                url: Some("https://univec.ai/legal/models/univec-commercial/2026-08-09".into()),
                acceptance: "notice".into(),
                models: vec!["converter-a".into()],
                acknowledgement_required: true,
                accept_flag: Some("--accept-license univec-commercial@2026-08-09".into()),
            },
        ];
        plan.push(PlanStep::InstallModel {
            name: "converter-a".into(),
            backend: "onnx-runtime".into(),
        });

        let described = plan.describe().join("\n");
        assert!(described.contains("terms in this plan:"), "{described}");
        assert!(
            described.contains("apache-2.0 — embed-a, embed-bridge"),
            "{described}"
        );
        assert!(
            described.contains("univec-commercial 2026-08-09 (notice) — converter-a"),
            "{described}"
        );
        assert!(
            described.contains("https://univec.ai/legal/models/univec-commercial/2026-08-09"),
            "{described}"
        );
        assert!(
            described.contains(
                "requires acknowledgement: --accept-license univec-commercial@2026-08-09"
            ),
            "{described}"
        );

        let value = serde_json::to_value(&plan).unwrap();
        assert_eq!(value["terms"][0]["license"], "apache-2.0");
        assert_eq!(value["terms"][0]["acceptance"], "none");
        assert!(value["terms"][0].get("version").is_none());
        assert_eq!(value["terms"][1]["version"], "2026-08-09");
        assert_eq!(value["terms"][1]["acknowledgement_required"], true);
        assert_eq!(
            value["terms"][1]["accept_flag"],
            "--accept-license univec-commercial@2026-08-09"
        );

        // An evidenced notice document says so instead of demanding a flag.
        let mut evidenced = plan.terms[1].clone();
        evidenced.acknowledgement_required = false;
        evidenced.accept_flag = None;
        let lines = evidenced.describe().join("\n");
        assert!(lines.contains("already acknowledged"), "{lines}");

        // Other commands' plans omit the field entirely.
        let bare = serde_json::to_value(Plan::new("setup", "18/main")).unwrap();
        assert!(bare.get("terms").is_none());
    }

    #[test]
    fn an_empty_plan_is_a_noop_and_says_so() {
        let plan = Plan::new("setup", "18/main");
        assert!(plan.is_noop());
        assert!(plan.headline().contains("already in the requested state"));
        // A no-op needs no confirmation at all, interactive or not.
        assert!(confirm(&plan, false, None, Prompt::non_interactive()).is_ok());
    }

    #[test]
    fn destructive_steps_mark_the_plan_and_shout_in_their_description() {
        let mut plan = Plan::new("uninstall", "18/main");
        plan.push(PlanStep::CleanupPostvecObjects {
            database: "univec".into(),
            drop_columns: true,
            drop_destinations: true,
        });
        assert!(plan.destructive);
        assert!(plan.steps[0].describe().contains("DROP THE SHADOW VECTOR"));
        assert!(plan.steps[0].describe().contains("CHUNK DESTINATIONS"));

        let mut keeping = Plan::new("uninstall", "18/main");
        keeping.push(PlanStep::CleanupPostvecObjects {
            database: "univec".into(),
            drop_columns: false,
            drop_destinations: false,
        });
        assert!(!keeping.destructive);
        assert!(keeping.steps[0].describe().contains("keeping"));
    }

    #[test]
    fn drop_extension_never_mentions_cascade_as_an_option() {
        let step = PlanStep::DropExtension {
            database: "univec".into(),
        };
        assert!(step.describe().contains("without CASCADE"));
    }

    #[test]
    fn a_non_interactive_mutation_without_yes_is_a_usage_error() {
        let error = confirm(&setup_plan(), false, None, Prompt::non_interactive()).unwrap_err();
        assert_eq!(error.exit(), Exit::Usage);
        assert!(error.to_string().contains("--yes"));
    }

    #[test]
    fn yes_is_enough_for_a_non_destructive_plan() {
        assert!(confirm(&setup_plan(), true, None, Prompt::non_interactive()).is_ok());
    }

    #[test]
    fn data_loss_needs_more_than_yes_when_nobody_can_be_asked() {
        let mut plan = Plan::new("uninstall", "18/main");
        plan.push(PlanStep::CleanupPostvecObjects {
            database: "univec".into(),
            drop_columns: true,
            drop_destinations: false,
        });
        // Nobody to ask and no --yes: refused before any mutation.
        let error = confirm(&plan, false, Some("univec"), Prompt::non_interactive()).unwrap_err();
        assert_eq!(error.exit(), Exit::Usage);
        assert!(error.to_string().contains("--acknowledge-data-loss"));
        // With --yes (which the caller only sets alongside the acknowledgement
        // flag) it proceeds without reading anything.
        assert!(confirm(&plan, true, Some("univec"), Prompt::non_interactive()).is_ok());
    }

    /// No confirmation path may read stdin unless it was told a human is
    /// there. This is what keeps `cargo test` from hanging in a terminal.
    #[test]
    fn a_non_interactive_prompt_never_reads_stdin() {
        let mut destructive = Plan::new("uninstall", "18/main");
        destructive.push(PlanStep::CleanupPostvecObjects {
            database: "univec".into(),
            drop_columns: true,
            drop_destinations: false,
        });
        for (plan, ack, yes) in [
            (setup_plan(), None, false),
            (setup_plan(), None, true),
            (destructive.clone(), Some("univec"), false),
            (destructive, Some("univec"), true),
        ] {
            // Each of these returns immediately; a blocking read would hang the
            // test run instead of failing it, so the assertion is the return.
            let _ = confirm(&plan, yes, ack, Prompt::non_interactive());
        }
    }

    #[test]
    fn the_environment_decides_interactivity_only_at_the_call_site() {
        assert!(!Prompt::non_interactive().interactive);
        // Whatever this process was started with, constructing it must not
        // read anything.
        let _ = Prompt::from_environment();
    }

    fn in_use_plan() -> Plan {
        let mut plan = Plan::new("model deactivate", "18/main");
        plan.push(PlanStep::AcknowledgeInUse {
            model: "baai-bge-m3".into(),
            columns: vec![InUseColumn {
                model: "baai-bge-m3".into(),
                declared_model: "baai-bge-m3".into(),
                database: "app".into(),
                relation: "public.docs".into(),
                column: "body".into(),
                state: "active".into(),
            }],
            unknown_databases: Vec::new(),
        });
        plan.push(PlanStep::SetModelEnabled {
            name: "baai-bge-m3".into(),
            enabled: false,
            dependency_of: None,
        });
        plan
    }

    /// `--yes` says "this is the change I meant". It cannot also say "I accept
    /// that these columns stop working", because the operator may not have
    /// known about them when they typed the command.
    #[test]
    fn yes_alone_never_acknowledges_columns_still_using_a_model() {
        let never = |_: &[String]| -> Result<String> { panic!("must not prompt") };
        let err = confirm_in_use(
            &in_use_plan(),
            false,
            true,
            false,
            Prompt::non_interactive(),
            never,
        )
        .unwrap_err();
        assert_eq!(err.exit(), Exit::Usage);
        assert!(err.to_string().contains("--acknowledge-in-use"), "{err}");

        // Both together proceed, without asking anything.
        confirm_in_use(
            &in_use_plan(),
            true,
            true,
            false,
            Prompt::non_interactive(),
            never,
        )
        .unwrap();
    }

    /// Interactively the operator types the names back. A mismatch changes
    /// nothing, and a plan with nothing in use asks nothing at all.
    #[test]
    fn the_interactive_acknowledgement_requires_the_exact_names() {
        let wrong = |_: &[String]| Ok("something else".to_string());
        let err = confirm_in_use(
            &in_use_plan(),
            false,
            false,
            false,
            Prompt::interactive(),
            wrong,
        )
        .unwrap_err();
        assert!(err.to_string().contains("did not match"), "{err}");
        // …and the refusal says what would have worked.
        assert!(err.to_string().contains("baai-bge-m3"), "{err}");

        let right = |models: &[String]| Ok(models.join(", "));
        confirm_in_use(
            &in_use_plan(),
            false,
            false,
            false,
            Prompt::interactive(),
            right,
        )
        .unwrap();

        // Nothing in use: no prompt, no flag, no error.
        let never = |_: &[String]| -> Result<String> { panic!("must not prompt") };
        let mut ordinary = Plan::new("model deactivate", "18/main");
        ordinary.push(PlanStep::SetModelEnabled {
            name: "m".into(),
            enabled: false,
            dependency_of: None,
        });
        confirm_in_use(&ordinary, false, false, false, Prompt::interactive(), never).unwrap();
    }

    /// The acknowledgement proves the operator read *which* models are
    /// affected, not that they can punctuate. What the prompt prints must be
    /// accepted verbatim — that is the first thing anyone types — and so must
    /// the shapes a copy-paste from the warning block produces.
    #[test]
    fn the_acknowledgement_accepts_what_the_prompt_shows() {
        let models = vec!["baai-bge-m3".to_string(), "embed-dep".to_string()];
        // Exactly what `interactive_in_use_acknowledgement` renders.
        assert!(in_use_answer_matches(&models.join(" "), &models));
        for accepted in [
            "baai-bge-m3 embed-dep\n",
            "baai-bge-m3,embed-dep",
            "baai-bge-m3, embed-dep",
            "  baai-bge-m3   embed-dep  ",
            // A paste that carried the quotes an earlier prompt printed.
            "\"baai-bge-m3\", \"embed-dep\"",
            // Order is not the point; the set is.
            "embed-dep baai-bge-m3",
        ] {
            assert!(
                in_use_answer_matches(accepted, &models),
                "must accept {accepted:?}"
            );
        }
        for refused in [
            "",
            "   ",
            "yes",
            // Naming only some of them is not an acknowledgement of all.
            "baai-bge-m3",
            "baai-bge-m3 embed-dep extra",
            "baai-bge-m3embed-dep",
        ] {
            assert!(
                !in_use_answer_matches(refused, &models),
                "must refuse {refused:?}"
            );
        }
    }

    /// A dry run asks nothing: the plan it just printed already carries the
    /// warning and the exact flag.
    #[test]
    fn a_dry_run_neither_prompts_nor_demands_the_flag() {
        let never = |_: &[String]| -> Result<String> { panic!("must not prompt") };
        confirm_in_use(
            &in_use_plan(),
            false,
            false,
            true,
            Prompt::interactive(),
            never,
        )
        .unwrap();
    }

    /// An acknowledgement step is not a mutation: a plan carrying only those
    /// must not be able to demand confirmation for work it is not doing.
    #[test]
    fn an_acknowledgement_alone_is_still_a_noop() {
        let mut plan = Plan::new("model deactivate", "18/main");
        plan.push(PlanStep::AcknowledgeInUse {
            model: "m".into(),
            columns: Vec::new(),
            unknown_databases: vec!["analytics".into()],
        });
        assert!(plan.is_noop());
        assert!(confirm(&plan, false, None, Prompt::non_interactive()).is_ok());
    }

    /// The persistence step says what it is: a disk change that outlives the
    /// process, not a runtime toggle.
    #[test]
    fn the_enabled_step_says_it_survives_a_restart() {
        let on = PlanStep::SetModelEnabled {
            name: "m".into(),
            enabled: true,
            dependency_of: Some("converter".into()),
        }
        .describe();
        assert!(on.contains("enabled"), "{on}");
        assert!(on.contains("survives a restart"), "{on}");
        assert!(on.contains("converter depends on it"), "{on}");

        let off = PlanStep::SetModelEnabled {
            name: "m".into(),
            enabled: false,
            dependency_of: None,
        }
        .describe();
        assert!(off.contains("disabled"), "{off}");
    }

    /// Installing is not activating, and the plan says so with the command
    /// that finishes the job.
    #[test]
    fn the_install_step_says_the_model_lands_deactivated() {
        let described = PlanStep::InstallModel {
            name: "m".into(),
            backend: "onnx-runtime".into(),
        }
        .describe();
        assert!(described.contains("deactivated"), "{described}");
        assert!(
            described.contains("postvec model activate m"),
            "{described}"
        );
    }

    #[test]
    fn journal_exit_codes_distinguish_total_partial_and_deferred() {
        let mut journal = ApplyJournal::default();
        assert_eq!(journal.exit(), Exit::Success);

        journal.succeeded("univec");
        assert_eq!(journal.exit(), Exit::Success);

        journal.restart_deferred = true;
        assert_eq!(journal.exit(), Exit::RestartRequired);

        journal.restart_deferred = false;
        journal.failed("analytics", &CliError::apply("boom"));
        assert_eq!(
            journal.exit(),
            Exit::Partial,
            "one database succeeded and one did not"
        );

        let mut total = ApplyJournal::default();
        total.failed("univec", &CliError::apply("boom"));
        assert_eq!(total.exit(), Exit::Failure);
    }

    /// Work left undone is not success. Uninstall that removes the SQL but
    /// cannot take the database out of the launcher configuration leaves a
    /// worker still connecting to it — exit 0 would say otherwise.
    #[test]
    fn unfinished_work_downgrades_the_outcome_to_partial() {
        let mut journal = ApplyJournal::default();
        journal.succeeded("univec");
        assert_eq!(journal.exit(), Exit::Success);

        journal.incomplete("the launcher configuration still names univec");
        assert_eq!(journal.exit(), Exit::Partial);
    }

    #[test]
    fn a_failure_outranks_unfinished_work() {
        let mut journal = ApplyJournal::default();
        journal.succeeded("univec");
        journal.incomplete("something was left undone");
        journal.failed("analytics", &CliError::apply("boom"));
        assert_eq!(journal.exit(), Exit::Partial);

        let mut total = ApplyJournal::default();
        total.incomplete("something was left undone");
        total.failed("univec", &CliError::apply("boom"));
        assert_eq!(total.exit(), Exit::Failure);
    }

    #[test]
    fn journal_records_the_failure_and_its_fix() {
        let mut journal = ApplyJournal::default();
        journal.failed(
            "univec",
            &CliError::apply("CREATE EXTENSION failed").with_fix("run as superuser"),
        );
        assert_eq!(journal.failed_databases[0].name, "univec");
        assert_eq!(
            journal.failed_databases[0].remediation.as_deref(),
            Some("run as superuser")
        );
    }
}
