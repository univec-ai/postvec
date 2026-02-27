//! Observed facts — the boundary between IO and judgement.
//!
//! Collectors (`cluster`, `db`, `engine`) produce these structures; checks
//! (`checks`) consume them and produce [`crate::checks::CheckResult`]s. Nothing
//! in `checks` performs IO, which is what makes the diagnosis logic testable
//! without a PostgreSQL cluster, and what keeps `setup`'s preflight, `setup`'s
//! smoke checks, and `doctor` looking at the same facts.

use crate::cli::Mode;
use crate::validate::{GrpcEndpoint, HttpEndpoint};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// How a cluster was selected, and what it is called.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterIdentity {
    /// `18/main` for postgresql-common clusters; a stable synthetic id
    /// otherwise.
    pub id: String,
    pub major: u32,
    /// `main`, or `explicit` for a `--pg-config` selection.
    pub name: String,
}

impl ClusterIdentity {
    /// Filesystem-safe key for the state and lock files.
    pub fn key(&self) -> String {
        format!("{}-{}", self.major, self.name.replace('/', "_"))
    }
}

/// Facts read from the running server.
///
/// The path fields are `Option` because `data_directory` and friends are
/// readable only by superusers and `pg_read_all_settings` members; `doctor` run
/// as an ordinary role still produces a useful report without them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerFacts {
    pub version_num: i32,
    pub version: String,
    pub data_directory: Option<String>,
    pub config_file: Option<String>,
    pub hba_file: Option<String>,
    pub port: i32,
    /// Text rendering; used only for equality across a restart.
    pub postmaster_start_time: String,
    /// The same instant, rendered explicitly in UTC with microsecond
    /// precision.
    ///
    /// Rendered with `to_char` rather than a plain cast because a cast depends
    /// on the session's `DateStyle` and `TimeZone`, which can differ per role
    /// or per database — two connections to the *same* server could then
    /// disagree about the text. This form cannot.
    #[serde(default)]
    pub postmaster_start_exact: Option<String>,
    /// `pg_control_system().system_identifier`, rendered as text.
    ///
    /// Identifies a replication *lineage*, not an instance: a physical standby
    /// or a restored copy carries its primary's identifier. Useful as one half
    /// of an identity proof, never as the whole of one. `None` when the
    /// connecting role may not read it.
    #[serde(default)]
    pub system_identifier: Option<String>,
}

impl ServerFacts {
    pub fn major(&self) -> u32 {
        (self.version_num / 10_000) as u32
    }

    /// Just the version number. `server_version` carries the whole packaging
    /// string (`18.4 (Ubuntu 18.4-1.pgdg22.04+1)`), which is too long for a
    /// header and adds nothing there.
    pub fn short_version(&self) -> &str {
        self.version
            .split_whitespace()
            .next()
            .unwrap_or(&self.version)
    }
}

/// One `pg_settings` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingRow {
    pub name: String,
    pub setting: String,
    pub context: String,
    pub source: String,
    pub sourcefile: Option<String>,
    pub sourceline: Option<i32>,
    pub pending_restart: bool,
}

/// One `pg_file_settings` row: every file that mentions a setting, including
/// the ones that lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSettingRow {
    pub name: String,
    pub setting: String,
    pub sourcefile: String,
    pub sourceline: i32,
    pub applied: bool,
    pub error: Option<String>,
}

/// The effective postvec-relevant configuration, as the server sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SettingsSnapshot {
    pub rows: Vec<SettingRow>,
    pub file_rows: Vec<FileSettingRow>,
}

impl SettingsSnapshot {
    pub fn get(&self, name: &str) -> Option<&SettingRow> {
        self.rows.iter().find(|r| r.name == name)
    }

    pub fn value(&self, name: &str) -> Option<&str> {
        self.get(name).map(|r| r.setting.as_str())
    }

    pub fn pending_restart(&self, name: &str) -> bool {
        self.get(name).is_some_and(|r| r.pending_restart)
    }

    /// The effective preload list, parsed.
    pub fn preload_items(&self) -> Vec<String> {
        crate::config::guc::parse_library_list(self.value("shared_preload_libraries").unwrap_or(""))
    }

    /// Databases the launcher is configured to serve.
    pub fn configured_databases(&self) -> Vec<String> {
        crate::config::guc::parse_extension_list(self.value("postvec.database").unwrap_or(""))
    }

    /// The mode the extension will actually use.
    ///
    /// An empty value means the default, and the default is **embedded**
    /// (`postvec/src/gucs.rs::parse_mode`). Reading it as remote would let
    /// `doctor` check the wrong half of the installation and call an
    /// unconfigured cluster healthy.
    ///
    /// `None` here means "the setting was not observed at all" — a cluster
    /// that has not loaded the library — which is different from an
    /// unconfigured one and is reported as such.
    pub fn mode(&self) -> Option<Mode> {
        match self.value("postvec.mode").map(str::trim) {
            None => None,
            Some("") | Some("embedded") => Some(Mode::Embedded),
            Some("grpc") => Some(Mode::Grpc),
            Some(_) => None,
        }
    }

    /// The raw `postvec.mode` value, for reporting an unparseable one.
    pub fn raw_mode(&self) -> Option<&str> {
        self.value("postvec.mode")
    }

    pub fn int(&self, name: &str, default: i64) -> i64 {
        self.value(name)
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(default)
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        match self.value(name) {
            Some("on") | Some("true") | Some("yes") | Some("1") => Some(true),
            Some("off") | Some("false") | Some("no") | Some("0") => Some(false),
            _ => None,
        }
    }

    pub fn grpc_endpoints(&self) -> Vec<String> {
        crate::config::guc::parse_extension_list(
            self.value("postvec.ninference_grpc_endpoints")
                .unwrap_or(""),
        )
    }

    pub fn http_endpoints(&self) -> Vec<String> {
        crate::config::guc::parse_extension_list(
            self.value("postvec.ninference_http_endpoints")
                .unwrap_or(""),
        )
    }

    pub fn embedded_models(&self) -> Vec<String> {
        crate::config::guc::parse_extension_list(
            self.value("postvec.embedded_models").unwrap_or(""),
        )
    }

    pub fn embedded_listen(&self) -> String {
        non_empty(self.value("postvec.embedded_listen"))
            .unwrap_or_else(|| crate::config::DEFAULT_EMBEDDED_LISTEN.to_string())
    }

    pub fn embedded_http_listen(&self) -> String {
        non_empty(self.value("postvec.embedded_http_listen"))
            .unwrap_or_else(|| crate::config::DEFAULT_EMBEDDED_HTTP_LISTEN.to_string())
    }

    pub fn ninference_path(&self) -> Option<PathBuf> {
        non_empty(self.value("postvec.ninference_path")).map(PathBuf::from)
    }

    /// The providers.d directory the embedded host reads (with the
    /// extension's default applied). A path, never a credential.
    pub fn providers_path(&self) -> PathBuf {
        non_empty(self.value("postvec.providers_path"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(crate::config::DEFAULT_PROVIDERS_PATH))
    }

    /// Config files the server reports as unparseable.
    pub fn file_errors(&self) -> Vec<&FileSettingRow> {
        self.file_rows
            .iter()
            .filter(|r| r.error.is_some())
            .collect()
    }
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A `pg_available_extensions` row joined to `pg_extension`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionAvailability {
    pub name: String,
    pub default_version: String,
    pub installed_version: Option<String>,
}

/// Package assets found (or not) in the selected cluster's directories.
#[derive(Debug, Clone, Serialize)]
pub struct AssetFacts {
    pub sharedir: PathBuf,
    pub pkglibdir: PathBuf,
    pub postvec_control: Option<PathBuf>,
    /// Install scripts found, e.g. `postvec--0.1.0.sql`.
    pub postvec_sql_versions: Vec<String>,
    pub postvec_library: Option<PathBuf>,
    pub vector_control: Option<PathBuf>,
    pub vector_library: Option<PathBuf>,
}

/// The extension's own report of itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub version: String,
    pub diagnostics_api: i64,
    pub embedded: bool,
    /// Model backends this `postvec.so` can actually load (e.g.
    /// `onnx-runtime`, `generic`). `None` on extension builds that predate
    /// the field. Compatibility is then unknown, not assumed.
    #[serde(default)]
    pub model_backends: Option<Vec<String>>,
}

/// The diagnostics contract this CLI was written against.
pub const SUPPORTED_DIAGNOSTICS_API: i64 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionFacts {
    /// `pg_extension.extversion`.
    pub catalog_version: String,
    /// `postvec.version()` — the loaded library's crate version.
    pub library_version: String,
    /// `postvec.build_info()`; absent on extension versions that predate it.
    pub build_info: Option<BuildInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkerFacts {
    pub pid: Option<i32>,
    pub last_beat_age_s: Option<f64>,
    pub started_at: Option<String>,
    pub last_error: Option<String>,
    pub errors: i64,
    pub jobs_embedded: i64,
    pub jobs_dead_lettered: i64,
    pub model_refreshes: i64,
    /// Whether the heartbeat pid is a live backend of this cluster.
    pub pid_is_live: bool,
    /// A second heartbeat sample, taken only under `--deep`.
    pub second_beat_age_s: Option<f64>,
    pub advanced: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueFacts {
    pub pending: i64,
    pub claimed: i64,
    pub dead: i64,
    pub oldest_pending_s: Option<f64>,
    /// Distinct recent dead-letter reasons, truncated for display.
    pub dead_reasons: Vec<String>,
}

fn default_index_mode() -> String {
    "manual".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntryFacts {
    pub registry_id: i64,
    pub relation: String,
    pub source_column: String,
    pub vector_column: String,
    pub model: String,
    pub dim: i32,
    pub state: String,
    pub pending_jobs: i64,
    pub dead_jobs: i64,
    pub has_vector_index: bool,
    /// `manual` | `immediate` | `auto` (`manual` when the extension
    /// predates the column).
    #[serde(default = "default_index_mode")]
    pub index_mode: String,
    /// The parked automatic-build failure, if any.
    #[serde(default)]
    pub index_error: Option<String>,
    /// Whether a valid/ready/live ANN index with the opclass `search()`
    /// expects for this entry's distance/dimension covers the vector column
    /// (ownership-neutral — a user index counts, without being claimed).
    #[serde(default)]
    pub has_expected_opclass_index: bool,
    pub last_error: Option<String>,
    pub relation_exists: bool,
    pub source_column_exists: bool,
    pub vector_column_exists: bool,
    pub trigger_count: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCacheFacts {
    pub count: i64,
    pub names: Vec<String>,
    pub newest_last_seen_s: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationFacts {
    pub id: i64,
    pub registry_id: i64,
    pub state: String,
    pub rows_done: i64,
    pub rows_total: i64,
    pub error: Option<String>,
    pub age_s: Option<f64>,
    pub retry_failures: i32,
}

/// Everything observable inside one database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseFacts {
    pub name: String,
    pub exists: bool,
    /// Why the database could not be inspected, if it could not be.
    pub unreachable: Option<String>,
    pub available: Vec<ExtensionAvailability>,
    pub postvec: Option<ExtensionFacts>,
    pub vector_version: Option<String>,
    pub worker: Option<WorkerFacts>,
    pub queue: Option<QueueFacts>,
    pub registry: Vec<RegistryEntryFacts>,
    pub models: ModelCacheFacts,
    pub migrations: Vec<MigrationFacts>,
}

impl DatabaseFacts {
    pub fn absent(name: &str) -> Self {
        Self {
            name: name.to_string(),
            exists: false,
            unreachable: None,
            available: Vec::new(),
            postvec: None,
            vector_version: None,
            worker: None,
            queue: None,
            registry: Vec::new(),
            models: ModelCacheFacts::default(),
            migrations: Vec::new(),
        }
    }

    pub fn availability(&self, name: &str) -> Option<&ExtensionAvailability> {
        self.available.iter().find(|a| a.name == name)
    }

    pub fn scope(&self) -> String {
        format!("database:{}", self.name)
    }
}

// --- Inference-side probes -------------------------------------------------

/// One model as advertised by a `/config` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventoryModel {
    pub name: String,
    pub enabled: bool,
    pub model_type: Option<String>,
    /// `Some(connector type)` when the host serves this name through an
    /// external provider rather than its own engine. `/config` carries both
    /// kinds in one list, so anything reasoning about "what this host runs
    /// locally" has to look at this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// A parsed ninference `/config` envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigInventory {
    pub models: Vec<InventoryModel>,
}

impl ConfigInventory {
    /// Names of models the node reports as enabled — the set that can actually
    /// serve traffic.
    pub fn enabled_names(&self) -> BTreeSet<String> {
        self.models
            .iter()
            .filter(|m| m.enabled)
            .map(|m| m.name.clone())
            .collect()
    }

    pub fn all_names(&self) -> BTreeSet<String> {
        self.models.iter().map(|m| m.name.clone()).collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GrpcProbe {
    pub endpoint: GrpcEndpoint,
    pub resolved: Vec<String>,
    pub resolve_error: Option<String>,
    pub connected: bool,
    pub connect_error: Option<String>,
    pub duration_ms: u64,
}

/// Outcome of the optional `/health` route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "kebab-case")]
pub enum HealthOutcome {
    Ok {
        status: u16,
    },
    /// The node does not implement the route. Not a failure.
    NotImplemented {
        status: u16,
    },
    Failed {
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct HttpProbe {
    pub endpoint: HttpEndpoint,
    pub health: HealthOutcome,
    pub config_status: Option<u16>,
    pub config: Option<ConfigInventory>,
    pub error: Option<String>,
    /// `Some(false)` means the certificate did not verify and was accepted
    /// only because the extension accepts it too.
    pub tls_verified: Option<bool>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RemoteProbe {
    pub grpc: Vec<GrpcProbe>,
    pub http: Vec<HttpProbe>,
    /// Configured endpoint values that are not valid at all — a hand-edited
    /// `postvec.ninference_*_endpoints` with a typo. Reported rather than
    /// silently skipped, because the extension skips them silently too.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub malformed: Vec<MalformedEndpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MalformedEndpoint {
    pub value: String,
    pub error: String,
}

impl RemoteProbe {
    /// The union of enabled model names advertised by nodes that answered.
    pub fn advertised_enabled(&self) -> BTreeSet<String> {
        self.http
            .iter()
            .filter_map(|p| p.config.as_ref())
            .flat_map(|c| c.enabled_names())
            .collect()
    }

    /// Model names advertised with materially different types by different
    /// nodes. The extension's configured endpoint order decides the winner,
    /// so a disagreement is worth naming.
    pub fn conflicting_models(&self) -> Vec<String> {
        let mut seen: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
        for probe in &self.http {
            if let Some(config) = &probe.config {
                for model in &config.models {
                    seen.entry(model.name.clone()).or_default().insert(
                        model
                            .model_type
                            .clone()
                            .unwrap_or_else(|| "unknown".to_string()),
                    );
                }
            }
        }
        seen.into_iter()
            .filter(|(_, types)| types.len() > 1)
            .map(|(name, _)| name)
            .collect()
    }
}

/// A TCP listener probe.
#[derive(Debug, Clone, Serialize)]
pub struct ListenerProbe {
    pub address: String,
    pub connected: bool,
    pub error: Option<String>,
}

/// One on-disk model descriptor (`ninference.hub.json`), read with a tolerant
/// schema so unknown fields never break diagnosis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskModel {
    /// `configuration.name` — the name the engine registers a scan-loaded
    /// model under.
    pub name: String,
    /// The containing directory's name — what an explicit
    /// `postvec.embedded_models` entry has to match, because the engine
    /// resolves a requested model by *path*, not by descriptor name.
    pub dir_name: String,
    pub enabled: bool,
    pub backend: Option<String>,
    pub dependencies: Vec<String>,
    pub path: PathBuf,
}

impl DiskModel {
    /// True when the descriptor's declared name differs from its directory
    /// name. Both are legal, but the two engine lookup paths then disagree
    /// about what the model is called, which is worth reporting.
    pub fn name_mismatch(&self) -> bool {
        self.name != self.dir_name
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DescriptorError {
    pub path: PathBuf,
    pub error: String,
}

/// The CLI-managed model store of an engine root, as observed for the
/// `models.*` doctor checks. Collected by `collect::model_store_facts`;
/// the checks stay pure over this.
#[derive(Debug, Clone, Serialize)]
pub struct ModelStoreFacts {
    pub root: PathBuf,
    pub models: Vec<StoredModelFacts>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredModelFacts {
    pub dir_name: String,
    pub descriptor_name: Option<String>,
    pub backend: String,
    pub enabled: bool,
    /// A valid receipt is present (CLI ownership).
    pub cli_owned: bool,
    /// A receipt exists but does not parse / has a future schema.
    pub receipt_error: Option<String>,
    /// Name and backend as the receipt records them, for drift detection.
    pub receipt_name: Option<String>,
    pub receipt_backend: Option<String>,
    /// The installed revision of this name, when the CLI owns the directory.
    pub receipt_revision: Option<u64>,
    /// `None` = not verified (integrity hashing runs only under `--deep`);
    /// `Some(vec![])` = verified clean.
    pub integrity_problems: Option<Vec<String>>,
}

/// Whether the selected registry channel answered, observed only under
/// `--deep` (a normal doctor run performs no network access for this).
#[derive(Debug, Clone, Serialize)]
pub struct RegistryProbeFacts {
    pub attempted: bool,
    pub channel: Option<String>,
    pub model_count: Option<usize>,
    /// Which credential source was selected, if any — never the key.
    pub credential_source: Option<String>,
    pub error: Option<String>,
    /// Head revision per catalogue name, so the report can say which
    /// installed models the registry offers a newer revision of. Empty when
    /// the probe did not run or did not answer.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub head_revisions: std::collections::BTreeMap<String, u64>,
}

/// Where the engine root came from — the CLI must not claim to know a value
/// the server merely inherited from its environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RootSource {
    Guc,
    /// Supplied by `doctor --ninference-path` for diagnosis only.
    Override,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct EmbeddedProbe {
    pub root: Option<PathBuf>,
    pub root_source: RootSource,
    pub root_error: Option<String>,
    pub models_dir: Option<PathBuf>,
    pub models_dir_error: Option<String>,
    /// Whether the PostgreSQL account can traverse and read the root.
    pub readable_by_server: Option<bool>,
    pub ort_libraries: Vec<PathBuf>,
    pub descriptors: Vec<DiskModel>,
    pub descriptor_errors: Vec<DescriptorError>,
    pub grpc_listener: Option<ListenerProbe>,
    pub http_listener: Option<ListenerProbe>,
    pub loaded: Option<ConfigInventory>,
    pub loaded_error: Option<String>,
}

impl EmbeddedProbe {
    pub fn new(root: Option<PathBuf>, root_source: RootSource) -> Self {
        Self {
            root,
            root_source,
            root_error: None,
            models_dir: None,
            models_dir_error: None,
            readable_by_server: None,
            ort_libraries: Vec::new(),
            descriptors: Vec::new(),
            descriptor_errors: Vec::new(),
            grpc_listener: None,
            http_listener: None,
            loaded: None,
            loaded_error: None,
        }
    }

    /// Models that should be loaded, given the configured selection.
    ///
    /// With an empty `postvec.embedded_models` the engine scan-loads every
    /// enabled descriptor. With an explicit list only those are requested;
    /// their engine-resolved dependencies also end up loaded, but the CLI must
    /// not *require* names the engine alone can resolve.
    pub fn expected_names(&self, requested: &[String]) -> BTreeSet<String> {
        if requested.is_empty() {
            self.descriptors
                .iter()
                .filter(|d| d.enabled)
                .map(|d| d.name.clone())
                .collect()
        } else {
            // An explicit request names a model directory; the engine registers
            // it under the descriptor's own name, so translate where the two
            // differ.
            requested
                .iter()
                .map(|want| {
                    self.descriptors
                        .iter()
                        .find(|d| &d.dir_name == want)
                        .map(|d| d.name.clone())
                        .unwrap_or_else(|| want.clone())
                })
                .collect()
        }
    }

    /// Model directory names (what `postvec.embedded_models` must contain).
    pub fn directory_names(&self) -> BTreeSet<String> {
        self.descriptors
            .iter()
            .map(|d| d.dir_name.clone())
            .collect()
    }
}

/// One database's model cache, kept separate from every other database's.
///
/// Reconciling a *union* of caches against an engine inventory is unsound: two
/// databases each holding half the models look complete together and are broken
/// individually.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseCache {
    pub database: String,
    pub models: BTreeSet<String>,
}

impl DatabaseCache {
    /// Build the per-database caches from collected facts, skipping databases
    /// with no extension (they have no cache to compare).
    pub fn from_facts(databases: &[DatabaseFacts]) -> Vec<Self> {
        databases
            .iter()
            .filter(|facts| facts.postvec.is_some())
            .map(|facts| Self {
                database: facts.name.clone(),
                models: facts.models.names.iter().cloned().collect(),
            })
            .collect()
    }
}

/// The three-way reconciliation embedded mode needs: what is on disk, what the
/// engine loaded, and what the database cached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InventoryDiff {
    pub expected_not_loaded: Vec<String>,
    pub loaded_not_cached: Vec<String>,
    pub cached_not_loaded: Vec<String>,
}

impl InventoryDiff {
    pub fn compute(
        expected: &BTreeSet<String>,
        loaded: &BTreeSet<String>,
        cached: &BTreeSet<String>,
    ) -> Self {
        Self {
            expected_not_loaded: expected.difference(loaded).cloned().collect(),
            loaded_not_cached: loaded.difference(cached).cloned().collect(),
            cached_not_loaded: cached.difference(loaded).cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(name: &str, value: &str) -> SettingRow {
        SettingRow {
            name: name.into(),
            setting: value.into(),
            context: "postmaster".into(),
            source: "configuration file".into(),
            sourcefile: Some("/etc/postgresql/18/main/conf.d/99-postvec.conf".into()),
            sourceline: Some(3),
            pending_restart: false,
        }
    }

    fn snapshot(pairs: &[(&str, &str)]) -> SettingsSnapshot {
        SettingsSnapshot {
            rows: pairs.iter().map(|(n, v)| setting(n, v)).collect(),
            file_rows: Vec::new(),
        }
    }

    /// The two settings have different grammars, and the preload one holds
    /// file names — so `PostVec` is read as `PostVec`, not folded to
    /// `postvec`. See `config::guc`.
    #[test]
    fn snapshot_reads_lists_with_the_right_grammar() {
        let s = snapshot(&[
            ("shared_preload_libraries", "pg_stat_statements, PostVec"),
            ("postvec.database", " univec , analytics "),
        ]);
        assert_eq!(s.preload_items(), ["pg_stat_statements", "PostVec"]);
        assert_eq!(s.configured_databases(), ["univec", "analytics"]);
    }

    #[test]
    fn mode_defaults_to_embedded_and_rejects_garbage() {
        // Not observed at all — a cluster that has not loaded the library.
        assert_eq!(snapshot(&[]).mode(), None);
        // Observed and empty means the extension's default, which is embedded.
        assert_eq!(
            snapshot(&[("postvec.mode", "")]).mode(),
            Some(Mode::Embedded)
        );
        assert_eq!(
            snapshot(&[("postvec.mode", "grpc")]).mode(),
            Some(Mode::Grpc)
        );
        assert_eq!(
            snapshot(&[("postvec.mode", "embedded")]).mode(),
            Some(Mode::Embedded)
        );
        assert_eq!(snapshot(&[("postvec.mode", "Embedded")]).mode(), None);
        assert_eq!(snapshot(&[("postvec.mode", "local")]).mode(), None);
    }

    #[test]
    fn listener_defaults_match_the_extension() {
        let s = snapshot(&[]);
        assert_eq!(s.embedded_listen(), "127.0.0.1:33433");
        assert_eq!(s.embedded_http_listen(), "127.0.0.1:33434");
        let explicit = snapshot(&[("postvec.embedded_listen", " 127.0.0.1:9 ")]);
        assert_eq!(explicit.embedded_listen(), "127.0.0.1:9");
    }

    #[test]
    fn major_version_derives_from_version_num() {
        let facts = ServerFacts {
            version_num: 180_004,
            version: "18.4 (Ubuntu 18.4-1.pgdg22.04+1)".into(),
            data_directory: Some("/var/lib/postgresql/18/main".into()),
            config_file: Some("/etc/postgresql/18/main/postgresql.conf".into()),
            hba_file: Some("/etc/postgresql/18/main/pg_hba.conf".into()),
            port: 5432,
            postmaster_start_time: "2026-07-30 10:00:00+00".into(),
            postmaster_start_exact: None,
            system_identifier: None,
        };
        assert_eq!(facts.major(), 18);
        assert_eq!(facts.short_version(), "18.4");
    }

    #[test]
    fn inventory_reports_enabled_models_only() {
        let inv = ConfigInventory {
            models: vec![
                InventoryModel {
                    name: "a".into(),
                    enabled: true,
                    model_type: Some("embed".into()),
                    provider: None,
                },
                InventoryModel {
                    name: "b".into(),
                    enabled: false,
                    model_type: Some("embed".into()),
                    provider: None,
                },
            ],
        };
        assert_eq!(inv.enabled_names(), ["a".to_string()].into());
        assert_eq!(inv.all_names().len(), 2);
    }

    #[test]
    fn expected_models_follow_the_selection_rule() {
        let mut probe = EmbeddedProbe::new(Some(PathBuf::from("/opt/nin")), RootSource::Guc);
        probe.descriptors = vec![
            DiskModel {
                name: "on".into(),
                dir_name: "on".into(),
                enabled: true,
                backend: Some("onnx-runtime".into()),
                dependencies: vec![],
                path: PathBuf::from("/opt/nin/models/onnx-runtime/on/ninference.hub.json"),
            },
            DiskModel {
                name: "off".into(),
                dir_name: "off".into(),
                enabled: false,
                backend: Some("onnx-runtime".into()),
                dependencies: vec![],
                path: PathBuf::from("/opt/nin/models/onnx-runtime/off/ninference.hub.json"),
            },
        ];
        // Scan-load: every enabled descriptor.
        assert_eq!(probe.expected_names(&[]), ["on".to_string()].into());
        // Explicit: exactly what was asked for, dependencies excluded (the
        // engine resolves those and reports them as loaded).
        assert_eq!(
            probe.expected_names(&["off".to_string()]),
            ["off".to_string()].into()
        );
    }

    #[test]
    fn inventory_diff_names_each_direction() {
        let expected = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let loaded = ["b", "c"].iter().map(|s| s.to_string()).collect();
        let cached = ["c", "d"].iter().map(|s| s.to_string()).collect();
        let diff = InventoryDiff::compute(&expected, &loaded, &cached);
        assert_eq!(diff.expected_not_loaded, ["a"]);
        assert_eq!(diff.loaded_not_cached, ["b"]);
        assert_eq!(diff.cached_not_loaded, ["d"]);

        let same: BTreeSet<String> = ["x".to_string()].into();
        let none = InventoryDiff::compute(&same, &same, &same);
        assert_eq!(none, InventoryDiff::default());
    }

    #[test]
    fn conflicting_models_are_detected_across_nodes() {
        let probe = RemoteProbe {
            grpc: vec![],
            http: vec![
                HttpProbe {
                    endpoint: crate::validate::http_endpoint("https://a").unwrap(),
                    health: HealthOutcome::Ok { status: 200 },
                    config_status: Some(200),
                    config: Some(ConfigInventory {
                        models: vec![InventoryModel {
                            name: "m".into(),
                            enabled: true,
                            model_type: Some("embed".into()),
                            provider: None,
                        }],
                    }),
                    error: None,
                    tls_verified: Some(true),
                    duration_ms: 1,
                },
                HttpProbe {
                    endpoint: crate::validate::http_endpoint("https://b").unwrap(),
                    health: HealthOutcome::Ok { status: 200 },
                    config_status: Some(200),
                    config: Some(ConfigInventory {
                        models: vec![InventoryModel {
                            name: "m".into(),
                            enabled: true,
                            model_type: Some("convert".into()),
                            provider: None,
                        }],
                    }),
                    error: None,
                    tls_verified: Some(true),
                    duration_ms: 1,
                },
            ],
            malformed: vec![],
        };
        assert_eq!(probe.conflicting_models(), ["m"]);
        assert_eq!(probe.advertised_enabled(), ["m".to_string()].into());
    }

    #[test]
    fn cluster_key_is_filesystem_safe() {
        let id = ClusterIdentity {
            id: "18/main".into(),
            major: 18,
            name: "main".into(),
        };
        assert_eq!(id.key(), "18-main");
    }
}
