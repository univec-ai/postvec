//! The `providers.d` configuration directory (feature `config`).
//!
//! One TOML file per provider; the files on disk are the complete serving
//! truth. Credentials resolve here — at load time, on the inference host —
//! and nowhere else: the resolved [`crate::ProviderConfig`] never leaves
//! this process, and no error message produced by this module ever carries
//! a secret value (paths and env-var *names* only).
//!
//! Loading rules (docs/external-providers.md §6.1):
//! - read `*.toml` in lexicographic order; skip dot-files;
//! - `enabled = false` files parse but serve nothing;
//! - a TOML file or referenced secret file with mode broader than 0600 is
//!   refused with a named error, never silently accepted;
//! - one broken file must not take down the others: per-file failures are
//!   reported alongside whatever loaded (the caller logs them);
//! - a duplicate public model name across files is refused, not resolved:
//!   which file wins would decide where a bound column's source text is sent.

use crate::ProviderConfig;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Per-provider outbound concurrency cap when the file does not set one.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;
/// Per-attempt HTTP timeout when the file does not set one.
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;
/// Provider items-per-request cap when the descriptor does not set one.
/// Conservative: every supported provider accepts at least this many.
pub const DEFAULT_MAX_BATCH: usize = 96;

/// Hard ceilings against a corrupt or hostile file. `max_concurrent` sizes a
/// semaphore and widens the hosts' ingress gate (§7.3), so it must not be
/// operator-unbounded.
const MAX_MAX_CONCURRENT: usize = 64;
const MAX_MODELS_PER_FILE: usize = 64;
const MAX_NAME_BYTES: usize = 128;
/// A `provider_model_id` goes into a request body and, for Bedrock, into a
/// signed URL path.
const MAX_MODEL_ID_BYTES: usize = 256;
/// The per-attempt HTTP timeout. Anything past a few minutes holds a
/// provider permit and a tower slot for longer than any caller's deadline.
const MAX_TIMEOUT_MS: u64 = 300_000;
/// Items per provider request. Bounds the response budget the connectors
/// compute, so it must not be operator-unbounded either.
const MAX_MAX_BATCH: usize = 4096;
/// pgvector's compile-time ceiling (`VECTOR_MAX_DIM`). A descriptor above it
/// cannot back a `vector` column at all, so accepting one only produces a
/// model that fails at `enable()` — and inflates every response envelope
/// sized from the declared dimension on the way there.
const MAX_DIM: u32 = 16_000;

/// Directory-wide ceilings. Per-file limits alone bound nothing: a thousand
/// well-formed files are a thousand times the heap, the semaphores and the
/// ingress width.
const MAX_PROVIDER_FILES: usize = 32;
const MAX_TOTAL_MODELS: usize = 256;
/// Sum of every file's `max_concurrent`. Both hosts add this to their tower
/// ingress limit, and each in-flight provider call may hold a bounded
/// response body, so this is the multiplier on the feature's whole memory
/// footprint.
const MAX_TOTAL_CONCURRENT: usize = 256;
/// A connector file is a few dozen lines. A secret is a token.
///
/// Public because the CLI reads provider files too, and a second copy of this
/// number is a second rulebook: the two disagreeing is exactly how `provider
/// add` came to write a file `provider test` then refused.
pub const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_SECRET_BYTES: u64 = 16 * 1024;

/// The connector types [`crate::new_embedding_backend`] implements, in the
/// canonical spelling. The CLI's `gemini`/`amazon` aliases fold into these
/// (see [`crate::catalog::canonical_provider`]); a file should carry the
/// canonical name and either is accepted.
pub const SUPPORTED_PROVIDERS: &[&str] = &[
    "openai",
    "openrouter",
    "mistral",
    "google",
    "cohere",
    "aws",
    "univec",
];

fn default_true() -> bool {
    true
}
fn default_max_concurrent() -> usize {
    DEFAULT_MAX_CONCURRENT
}
fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}
fn default_max_batch() -> usize {
    DEFAULT_MAX_BATCH
}

/// The on-disk TOML schema. `deny_unknown_fields` on purpose: a typo'd
/// credential field must be a load error, not a silently ignored key.
/// Deliberately NOT `Debug`: the inline secret fields would be one stray
/// format string away from a log line.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderFile {
    /// Connector type: openai | openrouter | mistral | google | cohere | aws
    /// (the CLI accepts "gemini"/"amazon" aliases and the factory honors
    /// them, but files should carry the canonical names).
    provider: String,
    #[serde(default = "default_true")]
    enabled: bool,

    // Key-authenticated providers: exactly one of the triad.
    api_key: Option<String>,
    api_key_file: Option<PathBuf>,
    api_key_env: Option<String>,

    base_url: Option<String>,
    /// Opt in to a plaintext `base_url` on a non-loopback host. Off by
    /// default: `http://` there puts the API key, and every document the
    /// column embeds, on the network in the clear.
    #[serde(default)]
    allow_insecure_transport: bool,
    #[serde(default = "default_max_concurrent")]
    max_concurrent: usize,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,

    // AWS: region plus either the bearer-token triad or the SigV4 pair
    // (each via its own file/env/inline triad).
    region: Option<String>,
    bearer_token: Option<String>,
    bearer_token_file: Option<PathBuf>,
    bearer_token_env: Option<String>,
    access_key_id: Option<String>,
    access_key_id_file: Option<PathBuf>,
    access_key_id_env: Option<String>,
    secret_access_key: Option<String>,
    secret_access_key_file: Option<PathBuf>,
    secret_access_key_env: Option<String>,

    #[serde(default)]
    models: Vec<ModelDescriptor>,
}

/// What a `[[models]]` entry serves. `embed` is the default and the only
/// kind most connectors offer; `convert` is hosted vector-space conversion
/// (UniVec's `/v1/convert`) and is refused at load for every other connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    #[default]
    Embed,
    Convert,
}

/// One provider-backed model as declared in a provider file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDescriptor {
    /// Public name served in `/config` (e.g. `openai-text-embedding-3-small`).
    pub name: String,
    /// The identifier the provider's API expects (e.g. `text-embedding-3-small`).
    /// For `kind = "convert"` this is the provider-side id of the TARGET
    /// space; the source's id is `provider_source_id`.
    pub provider_model_id: String,
    /// Vector dimension; authoritative for the response-shape check. For a
    /// converter this is the OUTPUT (target-space) dimension.
    pub dim: u32,
    /// Provider items-per-request cap; the gateway sub-batches to it.
    #[serde(default = "default_max_batch")]
    pub max_batch: usize,
    /// Advertised as `sequence_len` in the discovery descriptor. Embed only —
    /// a converter takes vectors, not tokens.
    pub max_tokens: Option<u32>,
    /// What this entry serves: `embed` (default) or `convert`.
    #[serde(default)]
    pub kind: ModelKind,
    /// Convert only: the provider-side id of the SOURCE space.
    pub provider_source_id: Option<String>,
    /// Convert only: the postvec-side public name of the source space — the
    /// resolver's vocabulary (what a bound column's `model` says), which is
    /// not the provider's. `postvec.migrate()` offers this converter to
    /// columns whose model matches it exactly.
    pub source_model: Option<String>,
    /// Convert only: the postvec-side public name of the target space.
    pub target_model: Option<String>,
    /// Convert only: dimension of the source space (`dim` is the target's).
    pub source_dim: Option<u32>,
}

impl ModelDescriptor {
    /// The route-identity id behind [`ServedBy::model_id`] and the `/config`
    /// descriptor's `provider_model_id` field.
    ///
    /// For an embed entry it is the provider's own id. For a converter the
    /// id folds in **everything that decides what the route means** — the
    /// provider-side pair, the source dimension, and the postvec-side pair —
    /// so that a same-name change of any of them compares as drift in
    /// `provider rm` and as divergence in the fleet check, exactly like an
    /// embed model's id or dim change would. One derivation, used by the
    /// structural view, the live descriptor, and the CLI's document reader;
    /// a second copy is how identities learn to disagree.
    pub fn route_model_id(&self) -> String {
        route_model_id(
            self.kind,
            &self.provider_model_id,
            self.provider_source_id.as_deref(),
            self.source_model.as_deref(),
            self.source_dim,
            self.target_model.as_deref(),
        )
    }
}

/// The derivation behind [`ModelDescriptor::route_model_id`], callable from
/// readers that hold the fields rather than the struct (the CLI reads TOML
/// documents it must not re-parse through this schema).
pub fn route_model_id(
    kind: ModelKind,
    provider_model_id: &str,
    provider_source_id: Option<&str>,
    source_model: Option<&str>,
    source_dim: Option<u32>,
    target_model: Option<&str>,
) -> String {
    match kind {
        ModelKind::Embed => provider_model_id.to_string(),
        ModelKind::Convert => format!(
            "{}->{} for {}[{}]->{}",
            provider_source_id.unwrap_or(""),
            provider_model_id,
            source_model.unwrap_or(""),
            source_dim.unwrap_or(0),
            target_model.unwrap_or(""),
        ),
    }
}

/// A provider file with every secret resolved to a value.
#[derive(Debug)]
pub struct ResolvedProvider {
    /// The file stem (`openai` for `openai.toml`) — the operator-facing name.
    pub name: String,
    /// Resolved connector configuration for the factory.
    pub config: ProviderConfig,
    pub max_concurrent: usize,
    pub timeout_ms: u64,
    pub models: Vec<ModelDescriptor>,
}

/// One file that failed to load. The message never contains a secret.
#[derive(Debug)]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// The outcome of scanning a providers.d directory. Per-file failures are
/// isolated: `providers` holds everything that loaded.
#[derive(Debug, Default)]
pub struct LoadOutcome {
    pub providers: Vec<ResolvedProvider>,
    pub errors: Vec<LoadError>,
}

/// Open a private file and read it, bounded.
///
/// One `open` decides everything, and every check is made against the *open
/// descriptor*. The previous shape — `symlink_metadata`, then a separate
/// `read_to_string(path)` — asked about one file and read another: anything
/// able to replace the path between the two calls got its own file read with
/// the first one's verdict. `O_NOFOLLOW` refuses a symlink at open, and
/// `fstat` on the returned descriptor cannot be raced at all.
///
/// The mode check does the ownership check implicitly: a `0600` file opens
/// only for its owner (or root, which is the host's other legitimate
/// identity), so a file this succeeds on is one the serving account owns.
///
/// `limit` bounds the read. `providers.d` is operator input to a long-lived
/// process that, in embedded mode, is the PostgreSQL launcher; a file is a
/// few dozen lines and a secret is a token, so neither needs to be able to
/// consume the heap.
fn read_private(path: &Path, limit: u64) -> Result<String, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            // ELOOP is what O_NOFOLLOW returns for a symlink; say so, because
            // "too many levels of symbolic links" reads as a broken path.
            Some(libc::ELOOP) => format!(
                "{} is a symlink; provider files and the secrets they name must be regular \
                 files (a symlink can point at a world-readable one)",
                path.display()
            ),
            _ => format!("cannot open {}: {e}", path.display()),
        })?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.nlink() != 1 {
        return Err(format!(
            "{} has {} hard links; a credential must not be reachable under a second name",
            path.display(),
            meta.nlink()
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is readable by other users (mode {mode:o}); chmod 600 it (and rotate the key \
             if others could have read it)",
            path.display()
        ));
    }
    if meta.size() > limit {
        return Err(format!(
            "{} is {} bytes, over the {limit}-byte ceiling for this file",
            path.display(),
            meta.size()
        ));
    }
    // Bounded independently of the stat: a file can grow between the two.
    let mut body = String::new();
    file.take(limit + 1)
        .read_to_string(&mut body)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if body.len() as u64 > limit {
        return Err(format!(
            "{} exceeds the {limit}-byte ceiling for this file",
            path.display()
        ));
    }
    Ok(body)
}

/// The providers.d directory itself, as a trust boundary.
///
/// Group- or world-**write** is refused, not warned about: anyone with write
/// access to this directory can drop in a connector file, and the host will
/// load it and start sending source text to whatever endpoint it names. That
/// is a strictly larger problem than the world-readable key file this module
/// already refuses. Read and execute bits only disclose which providers are
/// configured, which is doctor's business rather than a refusal.
pub fn validate_directory(dir: &Path, expected_uid: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot stat {}: {e}", dir.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "{} is a symlink; refusing to read credentials through it",
            dir.display()
        ));
    }
    if !meta.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(format!(
            "{} is writable by other users (mode {mode:o}); anyone who can write here can add \
             a provider file and choose where this host sends source text. chmod 700 it",
            dir.display()
        ));
    }
    // Mode alone is not enough when the reader is **root**: a 0700 directory
    // owned by another account passes every permission test and root can read
    // straight through it. The owner has to be the identity that is supposed
    // to own it — the serving account, or the cluster owner the CLI is acting
    // for — or root itself.
    let owner = meta.uid();
    let expected = expected_uid.unwrap_or_else(current_uid);
    if owner != 0 && owner != expected {
        return Err(format!(
            "{} is owned by uid {owner}, not {expected}; a credential directory owned by \
             another account is not this host's to read",
            dir.display()
        ));
    }
    validate_ancestry(dir, expected)
}

/// Every ancestor of the credential directory must be one that only root or
/// the expected identity can rewrite.
///
/// The leaf's own mode and owner say nothing about whether the *path to it*
/// can be rewritten. `postvec.providers_path` and `--providers-path` are
/// operator-configurable, so "it lives under /etc/postvec" is an assumption
/// and not a fact.
///
/// Two rules, and both are needed:
///
/// - **Not group- or world-writable**, unless sticky. Anyone with write
///   access to a directory can rename its children, so a `0777` parent means
///   the component below it can be swapped between this validation and the
///   enumeration that follows.
/// - **Owned by root or by `expected_uid`.** Permission bits alone miss the
///   case the fourth-pass audit named: a `0755` directory owned by *another*
///   account is writable by that account — its owner can replace the
///   component below it at will. This also closes the sticky-directory hole,
///   because a `1777` parent (`/tmp`) delegates exactly that power to each
///   child's owner, so the child has to be ours.
fn validate_ancestry(dir: &Path, expected_uid: u32) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mut cursor = dir.parent();
    while let Some(ancestor) = cursor {
        let meta = std::fs::symlink_metadata(ancestor)
            .map_err(|e| format!("cannot stat {}: {e}", ancestor.display()))?;
        let mode = meta.permissions().mode();
        let sticky = mode & 0o1000 != 0;
        if mode & 0o022 != 0 && !sticky {
            return Err(format!(
                "{} is writable by other users (mode {:o}), so the path to {} can be \
                 replaced. chmod go-w it",
                ancestor.display(),
                mode & 0o7777,
                dir.display()
            ));
        }
        let owner = meta.uid();
        if owner != 0 && owner != expected_uid {
            return Err(format!(
                "{} is owned by uid {owner}, not {expected_uid} or root; its owner can \
                 replace the path to {} at will, whatever the mode says",
                ancestor.display(),
                dir.display()
            ));
        }
        cursor = ancestor.parent();
    }
    Ok(())
}

/// The effective uid of this process.
fn current_uid() -> u32 {
    // Safety: `geteuid` is always successful and takes no arguments.
    unsafe { libc::geteuid() }
}

/// Exactly one source per secret triad. Checked separately from resolution
/// so the structural pass can report it without reading a credential.
fn check_exclusive(
    field: &str,
    inline: &Option<String>,
    file: &Option<PathBuf>,
    env: &Option<String>,
) -> Result<(), String> {
    let set = [inline.is_some(), file.is_some(), env.is_some()]
        .iter()
        .filter(|s| **s)
        .count();
    if set > 1 {
        return Err(format!(
            "{field}, {field}_file and {field}_env are mutually exclusive; set exactly one"
        ));
    }
    Ok(())
}

/// Resolve one secret from its inline/file/env triad. Exactly one source may
/// be set; a referenced file must be 0600 and is trimmed of trailing
/// whitespace (key files conventionally end with a newline). Returns
/// `Ok(None)` when the whole triad is absent — presence requirements are the
/// factory's per-provider business.
fn resolve_secret(
    field: &str,
    inline: &Option<String>,
    file: &Option<PathBuf>,
    env: &Option<String>,
) -> Result<Option<String>, String> {
    check_exclusive(field, inline, file, env)?;
    let value = if let Some(v) = inline {
        v.clone()
    } else if let Some(path) = file {
        read_private(path, MAX_SECRET_BYTES).map_err(|e| format!("{field}_file: {e}"))?
    } else if let Some(var) = env {
        std::env::var(var).map_err(|_| {
            format!("{field}_env names {var:?}, which is not set in this process's environment")
        })?
    } else {
        return Ok(None);
    };
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(format!("{field} resolved to an empty value"));
    }
    Ok(Some(value))
}

/// A `base_url` override has to be an absolute `http`/`https` URL with a
/// host and nothing after the path: every connector builds its endpoint by
/// concatenating its own suffix onto this string.
///
/// Checked at load rather than left to the request, because of how a bad
/// value would otherwise present. `reqwest` reports an unparseable URL from
/// `send()`, as an ordinary `reqwest::Error` — which the §6.4 mapping
/// classifies `UpstreamServiceUnavailable`, i.e. **Transient**. A permanent
/// typo would therefore look like a provider outage and be retried until the
/// queue dead-lettered the rows, with nothing anywhere naming the real
/// cause. A trailing `/` gets the same treatment: it yields a `//v1/…` path
/// that some gateways answer with a 404 that reads like a wrong model id.
pub fn validate_base_url(raw: &str) -> Result<(), String> {
    let url =
        reqwest::Url::parse(raw).map_err(|e| format!("base_url {raw:?} is not a URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "base_url {raw:?} must be http:// or https:// (got scheme {:?})",
            url.scheme()
        ));
    }
    if url.host_str().unwrap_or("").is_empty() {
        return Err(format!("base_url {raw:?} names no host"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!(
            "base_url {raw:?} must not carry a query string or fragment: the connector \
             appends its own path to it"
        ));
    }
    // The docs said userinfo was refused before the code did. It has no
    // meaning for any connector here — every one of them authenticates with a
    // header — so a `user:pass@` in a base_url is a credential written
    // somewhere nothing reads it, and one that leaks into any diagnostic that
    // ever prints a URL.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!(
            "base_url {raw:?} carries userinfo; connectors authenticate with a header, so a \
             user:password in the URL is a credential nothing reads"
        ));
    }
    if raw.ends_with('/') {
        return Err(format!(
            "base_url {raw:?} must not end with '/': the connector appends its own path, so \
             the trailing slash becomes a doubled one"
        ));
    }
    Ok(())
}

/// True when this `base_url` would put the provider credential on the
/// network in cleartext: plain `http` to something other than loopback.
///
/// Not a refusal — a plaintext OpenAI-compatible endpoint on a private
/// network is a real deployment, and loopback (a sidecar, a mock server) is
/// the ordinary test shape. It is reported: by the host at load, and by
/// `postvec doctor`, because "the bearer token crosses the network in the
/// clear" is not something to discover from a packet capture.
pub fn base_url_is_plaintext_offhost(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "http" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    // `host_str` keeps the brackets on an IPv6 literal.
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if matches!(host, "localhost" | "localhost.") {
        return false;
    }
    !host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// The credential shape each connector type requires.
///
/// The factory enforces the same thing one layer later, on *resolved values*,
/// and that stays as the last line. This pass works on the **declarations**,
/// which is the only thing a reader that resolves no secret can see — and it
/// is what makes a file the host will refuse a per-file *load* error, visible
/// in `provider ls` and `postvec doctor`, rather than a gateway-build failure
/// that only appears in the host's log. Without it the whole family of
/// misdiagnoses this crate already fixed once returns: doctor green,
/// `provider.served` blaming a reload, and the actual cause — a missing
/// `api_key_file`, or `provider = "opanai"` — visible nowhere the operator
/// looks.
/// A referenced secret path must be absolute: the CLI, the embedded launcher
/// and postvec-server all resolve it, and none of them share a working
/// directory. A relative path means three readers can disagree about which
/// file is the credential.
fn check_absolute(field: &str, path: &Option<PathBuf>) -> Result<(), String> {
    match path {
        Some(path) if !path.is_absolute() => Err(format!(
            "{field} {} must be an absolute path: the CLI, the launcher and postvec-server \
             resolve it from different working directories",
            path.display()
        )),
        _ => Ok(()),
    }
}

/// A dimension this provider's per-model rules accept, for the one moment
/// `provider add` has to validate a file whose real dimension the probe has
/// not measured yet.
///
/// The alternative was to leave those models *out* of the pre-probe document,
/// which is what a previous pass did — and it silently removed them from every
/// other structural check too. A brand-new file with one uncatalogued model
/// then failed with "no `[[models]]` entries" and could never reach dimension
/// discovery at all, while in an existing file the omitted entries skipped the
/// per-file count, the id-length rule and the duplicate-name rule and were
/// probed before any of them applied.
///
/// A placeholder is honest about what it is: it exists so the *other* rules
/// can run, and the real dimension is validated at the write. Every value here
/// satisfies [`validate_model_for_provider`], which the test below pins.
pub fn placeholder_dim(provider: &str, provider_model_id: &str) -> u32 {
    match crate::catalog::canonical_provider(provider).as_str() {
        "cohere" => COHERE_FIXED_DIMS
            .iter()
            .find(|(id, _)| *id == provider_model_id)
            .map(|(_, dim)| *dim)
            .unwrap_or(1024),
        "google" => crate::gemini::gemini_contract(provider_model_id)
            .and_then(|c| c.dims)
            .map(|(_, native)| native)
            .unwrap_or(3072),
        _ => 1536,
    }
}

/// Everything the loader can say about one `(provider, model id, dim)` on its
/// own, without seeing the rest of the file.
///
/// Split out so that **`provider add` and `provider test` can apply it before
/// spending a paid API call.** Those two build a backend straight from the
/// factory, which knows nothing about per-model contracts — so a descriptor
/// naming a Gemini model postvec cannot serve, or a Cohere width the model
/// does not produce, used to be probed (and billed) and only then refused at
/// the write. The rule has to live where both the loader and the CLI can
/// reach it, and this is that place.
///
/// `dim` is optional because `provider add` may not know it yet — the probe
/// is what measures an unknown one. The model-id rules apply either way.
pub fn validate_model_for_provider(
    provider: &str,
    provider_model_id: &str,
    dim: Option<u32>,
) -> Result<(), String> {
    match crate::catalog::canonical_provider(provider).as_str() {
        // Cohere's v4 models accept only these output widths, and the v3
        // models have a fixed one apiece. A descriptor asking for anything
        // else is a 400 on every call (v4) or a dimension mismatch on every
        // response (v3).
        "cohere" => {
            let Some(dim) = dim else { return Ok(()) };
            if provider_model_id.starts_with("embed-v4") && !COHERE_V4_DIMS.contains(&dim) {
                return Err(format!(
                    "dim {dim} is not one Cohere v4 produces ({}); the request would be \
                     refused on every call",
                    COHERE_V4_DIMS
                        .iter()
                        .map(|d| d.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some((_, fixed)) = COHERE_FIXED_DIMS
                .iter()
                .find(|(id, _)| *id == provider_model_id)
            {
                if dim != *fixed {
                    return Err(format!(
                        "{provider_model_id} always returns {fixed} components, not {dim}; \
                         every response would fail the dimension check"
                    ));
                }
            }
            Ok(())
        }
        // Checked against the **same table the client builds its request
        // from** (`gemini::GEMINI_CONTRACTS`), so the two cannot drift.
        // Google's embedding generations differ in whether they accept
        // `taskType` and `outputDimensionality` and in what widths they
        // produce; guessing does not fail, it silently changes every vector a
        // column stores.
        "google" => {
            let Some(contract) = crate::gemini::gemini_contract(provider_model_id) else {
                return Err(format!(
                    "Gemini model {provider_model_id:?} is not one whose request contract this \
                     postvec knows ({}). The generations differ in whether they accept \
                     taskType and outputDimensionality and in whether retrieval intent is a \
                     prompt prefix instead — guessing would embed your queries and documents \
                     identically while claiming otherwise. Use a listed model, or update \
                     postvec",
                    crate::gemini::GEMINI_CONTRACTS
                        .iter()
                        .map(|c| c.model_id)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            };
            match (contract.dims, dim) {
                (Some((low, high)), Some(dim)) if dim < low || dim > high => Err(format!(
                    "{} produces {low}..={high} components, not {dim}",
                    contract.model_id
                )),
                (None, Some(_)) => Err(format!(
                    "{} does not accept a requested output width, so `dim` must be its native \
                     one",
                    contract.model_id
                )),
                _ => Ok(()),
            }
        }
        _ => Ok(()),
    }
}

/// Cohere's v4 embedding models produce exactly these widths.
const COHERE_V4_DIMS: &[u32] = &[256, 512, 1024, 1536];

/// Cohere's v3 models have a **fixed** width apiece, so a descriptor is
/// either right or refused — there is nothing to negotiate and no field to
/// send. Enumerated here rather than left to the first insert, because "dim
/// is authoritative" means the gateway compares every response against it: a
/// wrong number turns into a dimension mismatch on every call, which reads
/// like a provider fault rather than a typo.
const COHERE_FIXED_DIMS: &[(&str, u32)] = &[
    ("embed-english-v3.0", 1024),
    ("embed-multilingual-v3.0", 1024),
    ("embed-english-light-v3.0", 384),
    ("embed-multilingual-light-v3.0", 384),
];

/// Everything the loader can say about one `kind = "convert"` entry on its
/// own — split out, like [`validate_model_for_provider`], so `provider add`
/// and `provider test` can apply the same rules before spending a paid call.
///
/// Hosted conversion is a UniVec capability. For every other connector a
/// converter entry describes a call the connector cannot make, so it is
/// refused at load rather than failing on every request.
pub fn validate_converter_for_provider(
    provider: &str,
    model: &ModelDescriptor,
) -> Result<(), String> {
    if crate::catalog::canonical_provider(provider) != "univec" {
        return Err(format!(
            "kind = \"convert\" is served only by provider \"univec\"; {provider:?} has no \
             conversion endpoint"
        ));
    }
    let source_id = model.provider_source_id.as_deref().unwrap_or("");
    if source_id.trim().is_empty() {
        return Err(
            "a converter needs provider_source_id: the provider-side id of the source space"
                .to_string(),
        );
    }
    if source_id.len() > MAX_MODEL_ID_BYTES {
        return Err(format!(
            "provider_source_id exceeds {MAX_MODEL_ID_BYTES} bytes"
        ));
    }
    let Some(source_model) = model.source_model.as_deref() else {
        return Err(
            "a converter needs source_model: the postvec-side public name of the source space \
             (what a bound column's model says — the resolver's vocabulary, not the provider's)"
                .to_string(),
        );
    };
    validate_model_name(source_model).map_err(|e| format!("source_model: {e}"))?;
    let Some(target_model) = model.target_model.as_deref() else {
        return Err(
            "a converter needs target_model: the postvec-side public name of the target space"
                .to_string(),
        );
    };
    validate_model_name(target_model).map_err(|e| format!("target_model: {e}"))?;
    if source_model == target_model {
        return Err(format!(
            "source_model and target_model are both {source_model:?}; a converter between a \
             space and itself converts nothing"
        ));
    }
    let Some(source_dim) = model.source_dim else {
        return Err("a converter needs source_dim: the source space's dimension".to_string());
    };
    if source_dim == 0 || source_dim > MAX_DIM {
        return Err(format!(
            "source_dim {source_dim} is outside 1..={MAX_DIM} (pgvector's VECTOR_MAX_DIM)"
        ));
    }
    if model.max_tokens.is_some() {
        return Err(
            "max_tokens applies only to embed entries; a converter takes vectors, not tokens"
                .to_string(),
        );
    }
    Ok(())
}

/// The AWS-only fields, refused for every other connector. Factored out so
/// the two key-authenticated arms cannot drift apart.
fn reject_aws_only_fields(
    file: &ProviderFile,
    region: bool,
    bearer: bool,
    access_key_id: bool,
    secret_access_key: bool,
) -> Result<(), String> {
    if region {
        return Err(format!(
            "region applies only to provider \"aws\"; {:?} ignores it",
            file.provider
        ));
    }
    if bearer || access_key_id || secret_access_key {
        return Err(format!(
            "bearer_token* and the AWS SigV4 fields apply only to provider \"aws\"; {:?} \
             authenticates with api_key*",
            file.provider
        ));
    }
    Ok(())
}

fn validate_connector(file: &ProviderFile) -> Result<(), String> {
    let declared = |inline: &Option<String>, path: &Option<PathBuf>, env: &Option<String>| {
        inline.is_some() || path.is_some() || env.is_some()
    };
    let api_key = declared(&file.api_key, &file.api_key_file, &file.api_key_env);
    let bearer = declared(
        &file.bearer_token,
        &file.bearer_token_file,
        &file.bearer_token_env,
    );
    let access_key_id = declared(
        &file.access_key_id,
        &file.access_key_id_file,
        &file.access_key_id_env,
    );
    let secret_access_key = declared(
        &file.secret_access_key,
        &file.secret_access_key_file,
        &file.secret_access_key_env,
    );

    // The schema is one flat table because TOML reads better that way, but it
    // describes a *tagged union*: `provider` selects which fields mean
    // anything. `deny_unknown_fields` cannot express that — every field below
    // is a known field — so a file can otherwise carry an `api_key` the AWS
    // connector never reads, or a `region` OpenAI ignores, and look
    // configured while behaving otherwise. Each arm therefore rejects the
    // fields its connector does not consume.
    for (field, path) in [
        ("api_key_file", &file.api_key_file),
        ("bearer_token_file", &file.bearer_token_file),
        ("access_key_id_file", &file.access_key_id_file),
        ("secret_access_key_file", &file.secret_access_key_file),
    ] {
        check_absolute(field, path)?;
    }

    let region_declared = file.region.is_some();

    match crate::catalog::canonical_provider(&file.provider).as_str() {
        "aws" => {
            if file.region.is_none() {
                return Err(
                    "provider \"aws\" needs `region`: the Bedrock endpoint is derived from it"
                        .to_string(),
                );
            }
            // The Titan connector builds its endpoint from the region alone
            // and never reads `base_url`; a file carrying one shows the
            // operator a setting the host ignores.
            if file.base_url.is_some() {
                return Err(
                    "base_url does not apply to provider \"aws\": the Bedrock endpoint comes \
                     from `region`"
                        .to_string(),
                );
            }
            if api_key {
                return Err(
                    "provider \"aws\" does not use api_key*: authenticate with a Bedrock \
                     bearer token or the access_key_id/secret_access_key pair"
                        .to_string(),
                );
            }
            match (bearer, access_key_id || secret_access_key) {
                (false, false) => {
                    return Err(
                        "provider \"aws\" needs either a Bedrock bearer token (bearer_token, \
                         bearer_token_file or bearer_token_env) or both access_key_id and \
                         secret_access_key"
                            .to_string(),
                    )
                }
                // The factory prefers the bearer token when both are present.
                // Silently, and there is no reading of a file carrying both
                // that makes the ignored half intentional.
                (true, true) => {
                    return Err(
                        "provider \"aws\" has both a bearer token and SigV4 credentials; the \
                         connector would use the bearer token and ignore the pair. Keep one"
                            .to_string(),
                    )
                }
                (true, false) => {}
                (false, true) => {
                    if !(access_key_id && secret_access_key) {
                        return Err("provider \"aws\" SigV4 needs both access_key_id and \
                             secret_access_key"
                            .to_string());
                    }
                }
            }
        }
        "cohere" => {
            if !api_key {
                return Err(format!(
                    "provider {:?} needs an API key: set exactly one of api_key_file \
                     (recommended), api_key_env or api_key",
                    file.provider
                ));
            }
            reject_aws_only_fields(
                file,
                region_declared,
                bearer,
                access_key_id,
                secret_access_key,
            )?;
            for model in &file.models {
                validate_model_for_provider("cohere", &model.provider_model_id, Some(model.dim))
                    .map_err(|e| format!("[[models]] {:?}: {e}", model.name))?;
            }
        }
        "google" => {
            if !api_key {
                return Err(format!(
                    "provider {:?} needs an API key: set exactly one of api_key_file \
                     (recommended), api_key_env or api_key",
                    file.provider
                ));
            }
            reject_aws_only_fields(
                file,
                region_declared,
                bearer,
                access_key_id,
                secret_access_key,
            )?;
            for model in &file.models {
                validate_model_for_provider("google", &model.provider_model_id, Some(model.dim))
                    .map_err(|e| format!("[[models]] {:?}: {e}", model.name))?;
            }
        }
        supported if SUPPORTED_PROVIDERS.contains(&supported) => {
            if !api_key {
                return Err(format!(
                    "provider {:?} needs an API key: set exactly one of api_key_file \
                     (recommended), api_key_env or api_key",
                    file.provider
                ));
            }
            reject_aws_only_fields(
                file,
                region_declared,
                bearer,
                access_key_id,
                secret_access_key,
            )?;
        }
        _ => {
            return Err(format!(
                "unsupported provider type {:?}; expected one of {} (aliases: gemini, amazon)",
                file.provider,
                SUPPORTED_PROVIDERS.join(", ")
            ))
        }
    }
    Ok(())
}

/// Public-name rule: the same charset the hosts accept for model names.
fn validate_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("model name is empty".to_string());
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!("model name exceeds {MAX_NAME_BYTES} bytes"));
    }
    let first = name.as_bytes()[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return Err(format!("model name {name:?} must start with [a-z0-9]"));
    }
    if !name
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "model name {name:?} contains characters outside [a-z0-9._-]"
        ));
    }
    Ok(())
}

/// Everything the loader checks **before** it touches a credential: the file
/// mode, the schema (`deny_unknown_fields`, so a typo'd key field is an
/// error rather than a silently ignored one), the per-file ceilings, the
/// descriptors, the region, and the one-source-per-secret rule. Duplicate
/// model names within the file are dropped here too, first-wins.
///
/// `Ok(false)` = `enabled = false`: the file parses and serves nothing, and
/// nothing past that point is checked — exactly as at load, so a caller
/// reporting on this cannot be stricter than the host.
///
/// Split out and public so `postvec doctor` can say "the serving host will
/// refuse this file, and here is why" using these rules rather than a second,
/// more permissive reader that would disagree — and without reading a single
/// secret.
pub fn validate_file(path: &Path) -> Result<bool, String> {
    let raw = read_private(path, MAX_FILE_BYTES)?;
    validate_str(&raw, &path.display().to_string())
}

/// [`validate_file`] without the file: the same rules over a rendered
/// document, so `postvec provider add` can refuse to *write* a file the host
/// would refuse to load. A connector file is refused as a whole, so one
/// mistaken entry takes that provider's already-working models down at the
/// next reload — and the command that composed it is the last place that can
/// still stop it.
pub fn validate_str(body: &str, label: &str) -> Result<bool, String> {
    let _ = label;
    // The size ceiling belongs here, not only in the reader. `validate_file`
    // gets it from `read_private`, so a *file* over the limit is refused —
    // but a rendered document was not measured at all, and `provider add`
    // could therefore write a connector the serving host (and `provider
    // test`, and `ls`, and doctor) would immediately refuse. Worse than
    // useless: it replaces a working configuration with one that fails at the
    // next restart.
    //
    // Size is the only rule the two paths differed on. Everything else
    // `read_private` checks — the `O_NOFOLLOW` open, regular-file type, link
    // count, mode — is a property of a file on disk, which a prospective
    // document does not have yet.
    if body.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "the file would be {} bytes, over the {MAX_FILE_BYTES}-byte ceiling for a \
             connector file",
            body.len()
        ));
    }
    let file: ProviderFile = toml::from_str(body).map_err(|e| format!("cannot parse: {e}"))?;
    if !file.enabled {
        return Ok(false);
    }
    validate_structure(&file)?;
    Ok(true)
}

fn parse_file(path: &Path) -> Result<ProviderFile, String> {
    let raw = read_private(path, MAX_FILE_BYTES)?;
    toml::from_str(&raw).map_err(|e| format!("cannot parse: {e}"))
}

/// Parse and resolve one provider file. `Ok(None)` = disabled (parses, serves
/// nothing).
fn load_file(path: &Path) -> Result<Option<ResolvedProvider>, String> {
    let file = parse_file(path)?;
    if !file.enabled {
        return Ok(None);
    }
    validate_structure(&file)?;

    let config = ProviderConfig {
        provider: file.provider.clone(),
        api_key: resolve_secret(
            "api_key",
            &file.api_key,
            &file.api_key_file,
            &file.api_key_env,
        )?,
        base_url: file.base_url.clone(),
        region: file.region.clone(),
        bearer_token: resolve_secret(
            "bearer_token",
            &file.bearer_token,
            &file.bearer_token_file,
            &file.bearer_token_env,
        )?,
        access_key_id: resolve_secret(
            "access_key_id",
            &file.access_key_id,
            &file.access_key_id_file,
            &file.access_key_id_env,
        )?,
        secret_access_key: resolve_secret(
            "secret_access_key",
            &file.secret_access_key,
            &file.secret_access_key_file,
            &file.secret_access_key_env,
        )?,
    };

    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| file.provider.clone());

    Ok(Some(ResolvedProvider {
        name,
        config,
        max_concurrent: file.max_concurrent,
        timeout_ms: file.timeout_ms,
        models: file.models,
    }))
}

fn validate_structure(file: &ProviderFile) -> Result<(), String> {
    if file.models.is_empty() {
        return Err("no [[models]] entries; an enabled provider must serve something".to_string());
    }
    if file.models.len() > MAX_MODELS_PER_FILE {
        return Err(format!(
            "{} [[models]] entries exceeds the {MAX_MODELS_PER_FILE} per-file ceiling",
            file.models.len()
        ));
    }
    if file.max_concurrent == 0 || file.max_concurrent > MAX_MAX_CONCURRENT {
        return Err(format!(
            "max_concurrent must be between 1 and {MAX_MAX_CONCURRENT}"
        ));
    }
    if file.timeout_ms == 0 || file.timeout_ms > MAX_TIMEOUT_MS {
        return Err(format!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"));
    }
    if let Some(region) = &file.region {
        crate::validate_region(region)?;
    }
    if let Some(base_url) = &file.base_url {
        validate_base_url(base_url)?;
        // Plaintext to something other than loopback is refused unless the
        // file says, in writing, that it is intended. A self-hosted
        // OpenAI-compatible endpoint on a private network is a real
        // deployment and stays possible — but "the API key and every embedded
        // document cross the network unencrypted" is not a thing to arrive at
        // by leaving out an `s`. Loopback (a sidecar, a mock) needs no
        // ceremony.
        if base_url_is_plaintext_offhost(base_url) && !file.allow_insecure_transport {
            return Err(format!(
                "base_url {base_url:?} is plain HTTP to a non-loopback host: the API key and \
                 every document embedded through it cross the network unencrypted. Use https, \
                 or set allow_insecure_transport = true to accept that deliberately"
            ));
        }
    }
    for model in &file.models {
        validate_model_name(&model.name).map_err(|e| format!("[[models]]: {e}"))?;
        if model.provider_model_id.trim().is_empty() {
            return Err(format!(
                "[[models]] {:?}: provider_model_id is empty",
                model.name
            ));
        }
        if model.provider_model_id.len() > MAX_MODEL_ID_BYTES {
            return Err(format!(
                "[[models]] {:?}: provider_model_id exceeds {MAX_MODEL_ID_BYTES} bytes",
                model.name
            ));
        }
        // The ceiling is pgvector's, not an arbitrary one: a descriptor above
        // it cannot back a `vector` column, so it can only produce a model
        // that fails at `enable()` — after inflating every response envelope
        // sized from the declared dimension along the way.
        if model.dim == 0 || model.dim > MAX_DIM {
            return Err(format!(
                "[[models]] {:?}: dim {} is outside 1..={MAX_DIM} (pgvector's VECTOR_MAX_DIM)",
                model.name, model.dim
            ));
        }
        if model.max_batch == 0 || model.max_batch > MAX_MAX_BATCH {
            return Err(format!(
                "[[models]] {:?}: max_batch must be between 1 and {MAX_MAX_BATCH}",
                model.name
            ));
        }
        // Advertised as `sequence_len`; zero would describe a model that can
        // embed nothing.
        if model.max_tokens == Some(0) {
            return Err(format!(
                "[[models]] {:?}: max_tokens must be positive when set",
                model.name
            ));
        }
        // The kind decides which of the remaining fields mean anything —
        // the same tagged-union rule the connector fields follow. An embed
        // entry carrying converter fields is a typo'd or half-edited file,
        // not a looser embed.
        match model.kind {
            ModelKind::Embed => {
                if model.provider_source_id.is_some()
                    || model.source_model.is_some()
                    || model.target_model.is_some()
                    || model.source_dim.is_some()
                {
                    return Err(format!(
                        "[[models]] {:?}: provider_source_id, source_model, target_model and \
                         source_dim apply only to kind = \"convert\"",
                        model.name
                    ));
                }
            }
            ModelKind::Convert => {
                validate_converter_for_provider(&file.provider, model)
                    .map_err(|e| format!("[[models]] {:?}: {e}", model.name))?;
            }
        }
    }
    // Same-file duplicates follow the same rule as cross-file ones, and for
    // the same reason: which entry wins decides which model a bound column's
    // text is embedded by. First-wins was deterministic and silent — the
    // losing entry simply vanished, so `provider add` could report two models
    // added and write one. One name, one entry, or the file is refused.
    let mut seen = std::collections::BTreeSet::new();
    for model in &file.models {
        if !seen.insert(model.name.clone()) {
            return Err(format!(
                "[[models]] defines {:?} more than once; a public name must have exactly one \
                 entry, because which one wins decides what a bound column is embedded by",
                model.name
            ));
        }
    }

    // One source per secret, checked here rather than only inside
    // `resolve_secret`, so a structural pass sees it too: a file with both
    // `api_key` and `api_key_env` is refused by the host, and a checker that
    // did not know that would call it healthy.
    check_exclusive(
        "api_key",
        &file.api_key,
        &file.api_key_file,
        &file.api_key_env,
    )?;
    check_exclusive(
        "bearer_token",
        &file.bearer_token,
        &file.bearer_token_file,
        &file.bearer_token_env,
    )?;
    check_exclusive(
        "access_key_id",
        &file.access_key_id,
        &file.access_key_id_file,
        &file.access_key_id_env,
    )?;
    check_exclusive(
        "secret_access_key",
        &file.secret_access_key,
        &file.secret_access_key_file,
        &file.secret_access_key_env,
    )?;
    // Last, because it is the rule that reads the others: which connector
    // this is, and whether the credential it needs is declared at all.
    validate_connector(file)
}

/// The directory-wide rules, applied to the directory **as it would be** after
/// `replaced` is written with `body`.
///
/// `validate_str` answers "will the host load this file?", and that was the
/// only question `provider add` asked. But the loader also refuses a
/// *directory* — over the file, model or aggregate-concurrency ceilings, or
/// holding a name two files claim — and when it does, the whole gateway comes
/// up empty at the next restart. A single valid file that pushes the set over
/// a ceiling is therefore a file the CLI must not write.
///
/// Only the structural pass runs per sibling (no secret is resolved), and a
/// sibling that fails it is skipped the way the loader would skip it, so this
/// predicts the host rather than being stricter than it.
pub fn validate_prospective_dir(dir: &Path, replaced: &Path, body: &str) -> Result<(), String> {
    evaluate_prospective(dir, replaced, Some(body), true)?.map(|_| ())
}

/// The public names the host would serve from `dir` **if** `replaced` held
/// `body` (or did not exist, for `None`), each with the file stem that owns
/// it. Exactly what the loader would serve: structural rules, the
/// contested-name rule, **and the directory ceilings** — a directory over a
/// ceiling is one the loader refuses, and a refused directory serves nothing.
///
/// That last clause is load-bearing for `provider rm`. An earlier version
/// modelled the contest rule but not the ceilings, so an over-ceiling
/// directory — which the host loads as *empty* — read as fully served, and
/// removing the file that repaired it showed no difference: every remaining
/// provider came online under plain `--yes`, with no recipient
/// acknowledgement anywhere.
///
/// `Err` is an I/O failure. A directory the host would refuse is `Ok(empty)`,
/// because that is what the host serves.
pub fn served_names_if(
    dir: &Path,
    replaced: &Path,
    body: Option<&str>,
) -> Result<std::collections::BTreeMap<String, ServedBy>, String> {
    Ok(evaluate_prospective(dir, replaced, body, false)?.unwrap_or_default())
}

/// A stable, secret-free digest of the endpoint a provider file will actually
/// reach: the AWS region and the `base_url` override, which together are the
/// only things that decide *where* a request goes once the connector type and
/// model id are fixed.
///
/// Hashed rather than published because `base_url` is operator-supplied and
/// routinely names an internal host, while `/config` is read by every node in
/// a fleet. A digest answers the only question a fleet needs to ask — "do all
/// the nodes reach the same place?" — without publishing the answer.
///
/// Lives in `config` rather than the gateway because the CLI computes it too:
/// `provider rm` compares the live snapshot's endpoint against the files'
/// to tell a same-name handoff from an unchanged route, and the two have to
/// hash identically.
pub fn endpoint_digest(region: Option<&str>, base_url: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(region.unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(base_url.unwrap_or("").as_bytes());
    hex::encode(hasher.finalize())[..16].to_string()
}

/// Who serves a public name: the connector — which is the **recipient** of
/// the text, and what a privacy acknowledgement has to name — and the file
/// stem an operator administers. A stem alone ("alpha") says nothing about
/// where text goes; two OpenAI-compatible endpoints in two files are the
/// same recipient, and a file called `alpha` can point at anyone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedBy {
    pub provider: String,
    pub file: String,
    /// [`endpoint_digest`] of the region and `base_url`. Two files of the
    /// same connector type are the same recipient only if this matches too.
    pub endpoint: String,
    /// The id the provider's API is asked for. The same public name backed
    /// by a different upstream model is a different route: the vectors are
    /// in a different space, and nothing downstream can tell.
    pub model_id: String,
    /// The declared dimension, for the same reason.
    pub dim: u32,
}

/// One evaluation behind both prospective questions, so they cannot disagree.
///
/// Outer `Err`: the directory cannot be scanned. Inner `Err`: the loader
/// would refuse the directory as a whole, with the reason. Inner `Ok`: the
/// names it would serve, each with its file stem.
///
/// `refuse_contests` is the one place the two questions differ. To the
/// loader a contested name is a per-file skip, not a directory refusal, and
/// `served_names_if` has to model that faithfully — including for a partial
/// `rm` whose remaining file is still contested with a sibling. A *write*
/// that would create a contest is a different matter: there is no reading of
/// it that the operator wants, so `validate_prospective_dir` refuses it.
#[allow(clippy::type_complexity)]
fn evaluate_prospective(
    dir: &Path,
    replaced: &Path,
    body: Option<&str>,
    refuse_contests: bool,
) -> Result<Result<std::collections::BTreeMap<String, ServedBy>, String>, String> {
    let (files, count) = prospective_set(dir, replaced, body)?;
    if count > MAX_PROVIDER_FILES {
        return Ok(Err(format!(
            "{} would hold {count} provider files, over the {MAX_PROVIDER_FILES}-file \
             ceiling; the host would refuse the whole directory",
            dir.display()
        )));
    }
    let contested = contested_names(files.iter().map(|(p, f)| (p.as_path(), &f.models)));
    // The loader counts toward the ceilings only what it accepts, and it
    // skips a contested file before counting it — so the same here.
    let accepted: Vec<&(PathBuf, ProviderFile)> = files
        .iter()
        .filter(|(_, f)| !f.models.iter().any(|m| contested.contains_key(&m.name)))
        .collect();
    let total_models: usize = accepted.iter().map(|(_, f)| f.models.len()).sum();
    if total_models > MAX_TOTAL_MODELS {
        return Ok(Err(format!(
            "{} would declare {total_models} provider models, over the {MAX_TOTAL_MODELS} \
             ceiling; the host would refuse the whole directory",
            dir.display()
        )));
    }
    let total_concurrent: usize = accepted.iter().map(|(_, f)| f.max_concurrent).sum();
    if total_concurrent > MAX_TOTAL_CONCURRENT {
        return Ok(Err(format!(
            "{} would declare {total_concurrent} total outbound concurrency, over the \
             {MAX_TOTAL_CONCURRENT} ceiling; the host would refuse the whole directory",
            dir.display()
        )));
    }
    if refuse_contests {
        if let Some((name, claimants)) = contested.iter().next() {
            return Ok(Err(format!(
                "model {name:?} would be declared by more than one file ({}); a public name \
                 must have exactly one owner, and the host would serve it from none of them",
                claimants
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    let mut served = std::collections::BTreeMap::new();
    for (path, file) in accepted {
        let provider = crate::catalog::canonical_provider(&file.provider);
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let endpoint = endpoint_digest(file.region.as_deref(), file.base_url.as_deref());
        for model in &file.models {
            served.insert(
                model.name.clone(),
                ServedBy {
                    provider: provider.clone(),
                    file: stem.clone(),
                    endpoint: endpoint.clone(),
                    // The ROUTE id, not the raw provider id: for a converter
                    // it folds in the provider-side pair, the source dim and
                    // the postvec-side pair, so a same-name change of any of
                    // them is drift here exactly as it is in the live
                    // descriptor (`descriptor_json` derives the same value).
                    model_id: model.route_model_id(),
                    dim: model.dim,
                },
            );
        }
    }
    Ok(Ok(served))
}

/// Every enabled, structurally valid file the directory would hold with
/// `replaced` substituted (or removed), plus the total file count including
/// the ones the structural pass skipped — the loader counts those too.
///
/// A `read_dir` entry error is an **error**, not an absence, for the same
/// reason it is in `load_dir`: swallowing it turns "half the directory is
/// unreadable" into "no siblings", and a preflight that cannot see a sibling
/// will approve a write that collides with it.
fn prospective_set(
    dir: &Path,
    replaced: &Path,
    body: Option<&str>,
) -> Result<(Vec<(PathBuf, ProviderFile)>, usize), String> {
    let mut files: Vec<(PathBuf, ProviderFile)> = Vec::new();
    let mut count = 0usize;
    if dir.is_dir() {
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in
            std::fs::read_dir(dir).map_err(|e| format!("cannot scan {}: {e}", dir.display()))?
        {
            let path = entry
                .map_err(|e| format!("cannot read an entry of {}: {e}", dir.display()))?
                .path();
            let named = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| !n.starts_with('.'));
            if path.extension().is_some_and(|ext| ext == "toml") && named && path != replaced {
                entries.push(path);
            }
        }
        entries.sort();
        for path in entries {
            count += 1;
            let Ok(raw) = read_private(&path, MAX_FILE_BYTES) else {
                continue;
            };
            let Ok(file) = toml::from_str::<ProviderFile>(&raw) else {
                continue;
            };
            if file.enabled && validate_structure(&file).is_ok() {
                files.push((path, file));
            }
        }
    }
    if let Some(body) = body {
        count += 1;
        let replacement: ProviderFile =
            toml::from_str(body).map_err(|e| format!("cannot parse: {e}"))?;
        if replacement.enabled {
            files.push((replaced.to_path_buf(), replacement));
        }
    }
    Ok((files, count))
}

/// Public names claimed by more than one file, with every file that claims
/// each. Shared by the loader and by `provider add`'s directory preflight so
/// the two cannot disagree about what a collision is.
fn contested_names<'a>(
    files: impl Iterator<Item = (&'a Path, &'a Vec<ModelDescriptor>)>,
) -> std::collections::BTreeMap<String, Vec<PathBuf>> {
    let mut claims: std::collections::BTreeMap<String, Vec<PathBuf>> = Default::default();
    for (path, models) in files {
        for model in models {
            claims
                .entry(model.name.clone())
                .or_default()
                .push(path.to_path_buf());
        }
    }
    claims.retain(|_, files| files.len() > 1);
    claims
}

/// Scan a providers.d directory. A missing directory is the zero-config
/// case: an empty outcome, no error. An existing-but-unreadable directory is
/// a structural error (`Err`), distinct from per-file failures.
pub fn load_dir(dir: &Path) -> Result<LoadOutcome, String> {
    let mut outcome = LoadOutcome::default();
    // The directory is the trust boundary, so it is validated **before**
    // anything inside it is enumerated — otherwise the scan and the checks
    // could describe different objects.
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(e) => return Err(format!("cannot stat {}: {e}", dir.display())),
        Ok(_) => validate_directory(dir, None)?,
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(outcome),
        Err(e) => return Err(format!("cannot scan {}: {e}", dir.display())),
    };
    // The directory is the trust boundary; check it before reading anything
    // inside it.

    // Lexicographic order makes the duplicate-name rule ("first wins")
    // deterministic across platforms and readdir orders.
    //
    // An entry that cannot be read is an error, not an absence. Swallowing it
    // turns "the directory is half-unreadable" into "no providers are
    // configured", which is indistinguishable from the ordinary zero-config
    // state and would silently take a working provider out of service.
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read an entry of {}: {e}", dir.display()))?;
        let path = entry.path();
        let named = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.'));
        if path.extension().is_some_and(|ext| ext == "toml") && named {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.len() > MAX_PROVIDER_FILES {
        return Err(format!(
            "{} holds {} provider files, over the {MAX_PROVIDER_FILES}-file ceiling",
            dir.display(),
            paths.len()
        ));
    }

    // Every file is parsed before any file is accepted, because the
    // duplicate-name rule is a property of the *set*. The earlier shape
    // refused only the later claimant and let the first one serve — which
    // meant removing the winning file quietly activated the other one's
    // endpoint for the same bound columns at the next reload, with no
    // acknowledgement anywhere. A name claimed by two files now takes
    // **both** of them out: one owner, or neither serves.
    let mut loaded: Vec<(PathBuf, ResolvedProvider)> = Vec::new();
    for path in paths {
        match load_file(&path) {
            Ok(Some(provider)) => loaded.push((path, provider)),
            Ok(None) => log::info!("providers.d: {} is disabled; skipping", path.display()),
            Err(message) => outcome.errors.push(LoadError { path, message }),
        }
    }
    let contested = contested_names(loaded.iter().map(|(path, p)| (path.as_path(), &p.models)));

    let mut total_models = 0usize;
    let mut total_concurrent = 0usize;
    for (path, provider) in loaded {
        if let Some(name) = provider
            .models
            .iter()
            .map(|m| &m.name)
            .find(|name| contested.contains_key(*name))
        {
            let others: Vec<String> = contested[name]
                .iter()
                .filter(|other| *other != &path)
                .map(|other| other.display().to_string())
                .collect();
            outcome.errors.push(LoadError {
                path,
                message: format!(
                    "model {name:?} is also declared by {}; a public name must have exactly \
                     one owner, because which file wins decides where a bound column's \
                     source text is sent. Neither file serves until one of them drops it",
                    others.join(", ")
                ),
            });
            continue;
        }
        // Directory-wide ceilings. Per-file limits bound one file; these
        // bound the process.
        if total_models + provider.models.len() > MAX_TOTAL_MODELS {
            return Err(format!(
                "{} declares more than {MAX_TOTAL_MODELS} provider models in total",
                dir.display()
            ));
        }
        if total_concurrent + provider.max_concurrent > MAX_TOTAL_CONCURRENT {
            return Err(format!(
                "{} declares more than {MAX_TOTAL_CONCURRENT} total outbound concurrency \
                 (the sum of every file's max_concurrent, which both hosts add to their \
                 gRPC ingress width)",
                dir.display()
            ));
        }
        total_models += provider.models.len();
        total_concurrent += provider.max_concurrent;
        outcome.providers.push(provider);
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A providers.d is 0700. `tempfile::tempdir()` honours the umask, so
    /// on a umask-002 host it would otherwise be group-writable — which the
    /// loader now refuses, correctly.
    fn private_tempdir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    fn write_mode(dir: &Path, name: &str, body: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    const OPENAI_TOML: &str = r#"
provider = "openai"
api_key = "sk-inline"
max_concurrent = 2

[[models]]
name = "openai-text-embedding-3-small"
provider_model_id = "text-embedding-3-small"
dim = 1536
max_batch = 512
max_tokens = 8191
"#;

    #[test]
    fn loads_a_valid_file_and_applies_defaults() {
        let dir = private_tempdir();
        write_mode(dir.path(), "openai.toml", OPENAI_TOML, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        assert_eq!(outcome.providers.len(), 1);
        let p = &outcome.providers[0];
        assert_eq!(p.name, "openai");
        assert_eq!(p.config.provider, "openai");
        assert_eq!(p.config.api_key.as_deref(), Some("sk-inline"));
        assert_eq!(p.max_concurrent, 2);
        assert_eq!(p.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(p.models[0].dim, 1536);
        assert_eq!(p.models[0].max_batch, 512);
        assert_eq!(p.models[0].max_tokens, Some(8191));
    }

    #[test]
    fn refuses_a_world_readable_toml() {
        let dir = private_tempdir();
        write_mode(dir.path(), "openai.toml", OPENAI_TOML, 0o644);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0]
                .message
                .contains("readable by other users"),
            "{}",
            outcome.errors[0]
        );
    }

    #[test]
    fn refuses_a_world_readable_key_file() {
        let dir = private_tempdir();
        let key = write_mode(dir.path(), "openai.key", "sk-from-file\n", 0o640);
        let body = format!(
            "provider = \"openai\"\napi_key_file = \"{}\"\n\n[[models]]\nname = \"m1\"\n\
             provider_model_id = \"m\"\ndim = 4\n",
            key.display()
        );
        write_mode(dir.path(), "openai.toml", &body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert!(
            outcome.errors[0]
                .message
                .contains("readable by other users"),
            "{}",
            outcome.errors[0]
        );
        // Errors must reference the path, never the key value.
        assert!(!outcome.errors[0].message.contains("sk-from-file"));
    }

    #[test]
    fn resolves_key_files_and_trims_trailing_newlines() {
        let dir = private_tempdir();
        let key = write_mode(dir.path(), "openai.key", "sk-from-file\n", 0o600);
        let body = format!(
            "provider = \"openai\"\napi_key_file = \"{}\"\n\n[[models]]\nname = \"m1\"\n\
             provider_model_id = \"m\"\ndim = 4\n",
            key.display()
        );
        write_mode(dir.path(), "openai.toml", &body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(
            outcome.providers[0].config.api_key.as_deref(),
            Some("sk-from-file")
        );
    }

    #[test]
    fn key_source_triad_is_mutually_exclusive() {
        let dir = private_tempdir();
        let body = "provider = \"openai\"\napi_key = \"a\"\napi_key_env = \"OPENAI_API_KEY\"\n\n\
                    [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        write_mode(dir.path(), "openai.toml", body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(
            outcome.errors[0].message.contains("mutually exclusive"),
            "{}",
            outcome.errors[0]
        );
    }

    #[test]
    fn a_missing_env_var_names_the_variable_not_a_value() {
        let dir = private_tempdir();
        let body = "provider = \"openai\"\napi_key_env = \"POSTVEC_TEST_NO_SUCH_VAR\"\n\n\
                    [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        write_mode(dir.path(), "openai.toml", body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(
            outcome.errors[0]
                .message
                .contains("POSTVEC_TEST_NO_SUCH_VAR"),
            "{}",
            outcome.errors[0]
        );
    }

    #[test]
    fn disabled_files_parse_but_serve_nothing() {
        let dir = private_tempdir();
        let body = OPENAI_TOML.replace(
            "provider = \"openai\"",
            "provider = \"openai\"\nenabled = false",
        );
        write_mode(dir.path(), "openai.toml", &body, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert!(outcome.errors.is_empty());
    }

    #[test]
    fn one_broken_file_does_not_take_down_the_others() {
        let dir = private_tempdir();
        write_mode(dir.path(), "aaa-broken.toml", "not toml at all", 0o600);
        write_mode(dir.path(), "openai.toml", OPENAI_TOML, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(outcome.providers.len(), 1, "the good file still loads");
        assert_eq!(outcome.errors.len(), 1);
    }

    /// One public name, one owner — and when two files claim it, **neither**
    /// serves. An earlier version refused only the later claimant and let the
    /// first one keep serving, which meant removing the winning file quietly
    /// activated the other endpoint for the same bound columns at the next
    /// reload, with no acknowledgement anywhere on that path.
    #[test]
    fn a_name_claimed_by_two_files_takes_both_out() {
        let dir = private_tempdir();
        let a = "provider = \"openai\"\napi_key = \"a\"\n\n[[models]]\nname = \"shared-name\"\n\
                 provider_model_id = \"first\"\ndim = 4\n";
        let b = "provider = \"mistral\"\napi_key = \"b\"\n\n[[models]]\nname = \"shared-name\"\n\
                 provider_model_id = \"second\"\ndim = 8\n";
        write_mode(dir.path(), "a.toml", a, 0o600);
        write_mode(dir.path(), "b.toml", b, 0o600);

        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty(), "neither claimant serves");
        assert_eq!(outcome.errors.len(), 2, "{:?}", outcome.errors);
        for error in &outcome.errors {
            assert!(error.message.contains("exactly one owner"), "{error}");
        }

        // The failure this rule exists to prevent: removing one file must not
        // quietly bring the other online. Here it does come online — and
        // that is now a *visible* operator action (deleting a file), not a
        // side effect of lexicographic order while both were present.
        std::fs::remove_file(dir.path().join("a.toml")).unwrap();
        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(outcome.providers.len(), 1);
        assert!(outcome.errors.is_empty());
    }

    /// `served_names_if` has to model the ceilings, not only the contest
    /// rule. An over-ceiling directory is one the loader refuses, and a
    /// refused directory serves **nothing** — so removing the file that
    /// repairs it activates every remaining provider. Without the ceilings
    /// in the model, "before" read as fully served and the diff was empty.
    #[test]
    fn served_names_if_models_the_ceilings_so_a_repairing_removal_shows_what_it_activates() {
        let dir = private_tempdir();
        let one = |name: &str, concurrent: usize| {
            format!(
                "provider = \"openai\"\napi_key = \"k\"\nmax_concurrent = {concurrent}\n\n\
                 [[models]]\nname = \"{name}\"\nprovider_model_id = \"{name}\"\ndim = 4\n"
            )
        };
        // Five files at 64 = 320 total concurrency: over the 256 ceiling,
        // so the host loads nothing.
        for i in 0..5 {
            write_mode(
                dir.path(),
                &format!("p{i}.toml"),
                &one(&format!("m{i}"), 64),
                0o600,
            );
        }
        let outcome = load_dir(dir.path());
        assert!(outcome.is_err(), "the loader refuses the whole directory");

        let victim = dir.path().join("p4.toml");
        let now = served_names_if(dir.path(), &victim, Some(&one("m4", 64))).unwrap();
        assert!(now.is_empty(), "over the ceiling, nothing is served now");

        let after = served_names_if(dir.path(), &victim, None).unwrap();
        assert_eq!(
            after.len(),
            4,
            "removing one file brings every other one online"
        );
        for name in ["m0", "m1", "m2", "m3"] {
            let by = after.get(name).expect(name);
            assert_eq!(by.file, format!("p{}", &name[1..]));
            assert_eq!(by.provider, "openai", "the recipient, not only the file");
        }
    }

    /// `provider add` checks the directory as it *would be*, not only the one
    /// file: a valid file that pushes the set over a ceiling, or that claims a
    /// name a sibling owns, makes the loader refuse the whole directory — and
    /// the gateway comes up empty at the next restart.
    #[test]
    fn the_prospective_directory_is_held_to_the_loaders_aggregate_rules() {
        let dir = private_tempdir();
        let one = |name: &str, concurrent: usize| {
            format!(
                "provider = \"openai\"\napi_key = \"k\"\nmax_concurrent = {concurrent}\n\n\
                 [[models]]\nname = \"{name}\"\nprovider_model_id = \"{name}\"\ndim = 4\n"
            )
        };
        // Siblings that together sit just under the concurrency ceiling.
        for i in 0..4 {
            write_mode(
                dir.path(),
                &format!("p{i}.toml"),
                &one(&format!("m{i}"), 60),
                0o600,
            );
        }
        let fresh = dir.path().join("new.toml");

        // A legal file that tips the sum over.
        let refused = validate_prospective_dir(dir.path(), &fresh, &one("m-new", 20)).unwrap_err();
        assert!(refused.contains("total outbound concurrency"), "{refused}");
        // The same file, small enough, is fine.
        assert!(validate_prospective_dir(dir.path(), &fresh, &one("m-new", 16)).is_ok());

        // A name a sibling already owns.
        let refused = validate_prospective_dir(dir.path(), &fresh, &one("m0", 1)).unwrap_err();
        assert!(refused.contains("more than one file"), "{refused}");

        // Replacing an existing file is measured as a replacement, not as an
        // addition: rewriting p0 with the same name is not a collision with
        // itself.
        let p0 = dir.path().join("p0.toml");
        assert!(validate_prospective_dir(dir.path(), &p0, &one("m0", 60)).is_ok());

        // The file count.
        let dir = private_tempdir();
        for i in 0..MAX_PROVIDER_FILES {
            write_mode(
                dir.path(),
                &format!("p{i:02}.toml"),
                &one(&format!("m{i}"), 1),
                0o600,
            );
        }
        let refused =
            validate_prospective_dir(dir.path(), &dir.path().join("one-more.toml"), &one("x", 1))
                .unwrap_err();
        assert!(refused.contains("provider files"), "{refused}");
    }

    /// One name, one entry — within a file as well as across them. First-wins
    /// was deterministic and *silent*: the losing entry simply vanished, so
    /// `provider add` could report two models added and write one, and which
    /// entry survived decided what a bound column was embedded by.
    #[test]
    fn a_duplicate_public_name_within_one_file_is_refused() {
        let dir = private_tempdir();
        let body = "provider = \"openai\"\napi_key = \"k\"\n\n\
                    [[models]]\nname = \"dup\"\nprovider_model_id = \"first\"\ndim = 4\n\n\
                    [[models]]\nname = \"dup\"\nprovider_model_id = \"second\"\ndim = 8\n";
        let path = write_mode(dir.path(), "openai.toml", body, 0o600);

        let refused = validate_file(&path).unwrap_err();
        assert!(refused.contains("more than once"), "{refused}");
        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert_eq!(outcome.errors.len(), 1);
    }

    #[test]
    fn missing_directory_is_zero_config_not_an_error() {
        let dir = private_tempdir();
        let missing = dir.path().join("providers.d");
        let outcome = load_dir(&missing).unwrap();
        assert!(outcome.providers.is_empty() && outcome.errors.is_empty());
    }

    #[test]
    fn dot_files_and_non_toml_are_skipped() {
        let dir = private_tempdir();
        write_mode(dir.path(), ".hidden.toml", "garbage", 0o600);
        write_mode(dir.path(), "README.md", "docs", 0o644);
        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty() && outcome.errors.is_empty());
    }

    #[test]
    fn unknown_fields_are_a_load_error() {
        // A typo'd credential field must fail loudly, not be ignored.
        let dir = private_tempdir();
        let body = OPENAI_TOML.replace("api_key =", "api_kee =");
        write_mode(dir.path(), "openai.toml", &body, 0o600);
        let outcome = load_dir(dir.path()).unwrap();
        assert_eq!(outcome.errors.len(), 1);
        assert!(
            outcome.errors[0].message.contains("api_kee"),
            "{}",
            outcome.errors[0]
        );
    }

    /// The region is interpolated into the Bedrock hostname and into the
    /// SigV4 credential scope, so a value that can carry a dot or a slash
    /// could point signed requests at another host.
    #[test]
    fn a_region_that_could_redirect_the_endpoint_is_refused() {
        let dir = private_tempdir();
        write_mode(
            dir.path(),
            "aws.toml",
            "provider = \"aws\"\nregion = \"us-east-1.evil.example\"\nbearer_token = \"t\"\n\n\
             [[models]]\nname = \"aws-titan-embed-text-v1\"\n\
             provider_model_id = \"amazon.titan-embed-text-v1\"\ndim = 1536\n",
            0o600,
        );
        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert!(
            outcome.errors[0].message.contains("region"),
            "{}",
            outcome.errors[0]
        );
    }

    /// `validate_file` gets the byte ceiling from the reader that opens the
    /// file. `validate_str` measures a rendered document, so it has to carry
    /// the same ceiling itself — otherwise `provider add` composes and writes
    /// a connector the host then refuses.
    #[test]
    fn the_byte_ceiling_applies_to_a_rendered_document_too() {
        let head = "provider = \"openai\"\napi_key = \"k\"\nbase_url = \"https://x/PADDING\"\n\n\
                    [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        let under = head.replace(
            "PADDING",
            &"a".repeat(MAX_FILE_BYTES as usize - head.len() - 32),
        );
        assert!((under.len() as u64) < MAX_FILE_BYTES);
        assert_eq!(validate_str(&under, "under"), Ok(true));

        let over = head.replace("PADDING", &"a".repeat(MAX_FILE_BYTES as usize));
        assert!((over.len() as u64) > MAX_FILE_BYTES);
        let refused = validate_str(&over, "over").unwrap_err();
        assert!(refused.contains("byte ceiling"), "{refused}");

        // And the two entry points agree on the same file.
        let dir = private_tempdir();
        let path = write_mode(dir.path(), "openai.toml", &over, 0o600);
        assert!(validate_file(&path).is_err());
    }

    /// `validate_file` is what the CLI reports from, so it must agree with
    /// the loader exactly — no stricter, no laxer — and it must never read a
    /// secret to reach its verdict.
    #[test]
    fn validate_file_agrees_with_the_loader_without_reading_a_secret() {
        let dir = private_tempdir();

        // A key file that would be refused if anything resolved it. The
        // structural pass must not care: it never looks at a key source's
        // contents, only at the shape of the declaration.
        let key = write_mode(dir.path(), "leaky.key", "sk-secret-value", 0o644);
        let body = format!(
            "provider = \"openai\"\napi_key_file = \"{}\"\n\n[[models]]\nname = \"m1\"\n\
             provider_model_id = \"m\"\ndim = 4\n",
            key.display()
        );
        let path = write_mode(dir.path(), "openai.toml", &body, 0o600);
        assert_eq!(validate_file(&path), Ok(true), "structure is fine");
        // …while the loader, which does resolve it, refuses.
        assert!(load_file(&path).is_err(), "the key file is world-readable");
        std::fs::remove_file(&path).unwrap();

        // Cases the loader refuses that a permissive TOML read would not.
        for (marker, body) in [
            (
                "unknown field",
                "provider = \"openai\"\napi_kee = \"k\"\n\n[[models]]\nname = \"m1\"\n\
                 provider_model_id = \"m\"\ndim = 4\n",
            ),
            (
                "two key sources",
                "provider = \"openai\"\napi_key = \"k\"\napi_key_env = \"OPENAI_API_KEY\"\n\n\
                 [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n",
            ),
            (
                "bad public name",
                "provider = \"openai\"\napi_key = \"k\"\n\n[[models]]\nname = \"Bad Name\"\n\
                 provider_model_id = \"m\"\ndim = 4\n",
            ),
            ("no models", "provider = \"openai\"\napi_key = \"k\"\n"),
        ] {
            let path = write_mode(dir.path(), "p.toml", body, 0o600);
            assert!(validate_file(&path).is_err(), "case {marker}");
            assert!(load_file(&path).is_err(), "case {marker} (loader)");
            std::fs::remove_file(&path).unwrap();
        }

        // A disabled file is checked no further than the loader checks it —
        // otherwise a checker would report problems the host never sees.
        let path = write_mode(
            dir.path(),
            "off.toml",
            "provider = \"openai\"\nenabled = false\n",
            0o600,
        );
        assert_eq!(validate_file(&path), Ok(false));
        assert!(load_file(&path).unwrap().is_none());
    }

    /// The factory refuses an unsupported connector type and a connector
    /// with no credential — but it does so at gateway build, where only the
    /// host's log sees it. Checking the *declarations* here is what makes
    /// those two a per-file load error, which is what `provider ls` and
    /// `postvec doctor` report from.
    #[test]
    fn a_connector_the_factory_cannot_build_is_a_file_error() {
        let dir = private_tempdir();
        let models = "\n\n[[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        for (marker, body, expect) in [
            (
                "unknown type",
                format!("provider = \"opanai\"\napi_key = \"k\"{models}"),
                "unsupported provider type",
            ),
            (
                "no key source",
                format!("provider = \"openai\"{models}"),
                "needs an API key",
            ),
            (
                "aws without a region",
                format!("provider = \"aws\"\nbearer_token = \"t\"{models}"),
                "needs `region`",
            ),
            (
                "aws without any credential",
                format!("provider = \"aws\"\nregion = \"us-east-1\"{models}"),
                "bearer token",
            ),
            (
                "aws with half a sigv4 pair",
                format!(
                    "provider = \"aws\"\nregion = \"us-east-1\"\naccess_key_id = \"AKIA\"{models}"
                ),
                "SigV4 needs both",
            ),
            (
                // The Titan connector never reads base_url; a file carrying
                // one shows a setting the host silently ignores.
                "aws with a base_url",
                format!(
                    "provider = \"aws\"\nregion = \"us-east-1\"\nbearer_token = \"t\"\n\
                     base_url = \"https://example.invalid\"{models}"
                ),
                "does not apply to provider \"aws\"",
            ),
        ] {
            let path = write_mode(dir.path(), "p.toml", &body, 0o600);
            let refused = validate_file(&path).unwrap_err();
            assert!(refused.contains(expect), "case {marker}: {refused}");
            assert!(load_file(&path).is_err(), "case {marker} (loader)");
            std::fs::remove_file(&path).unwrap();
        }

        // The shapes that do build: every key-authenticated type, and both
        // AWS credential variants.
        for (marker, body) in [
            ("openai", "provider = \"openai\"\napi_key_env = \"K\""),
            (
                "amazon alias",
                "provider = \"amazon\"\nregion = \"us-east-1\"\nbearer_token = \"t\"",
            ),
            (
                "aws sigv4",
                "provider = \"aws\"\nregion = \"us-east-1\"\naccess_key_id = \"AKIA\"\n\
                 secret_access_key = \"s\"",
            ),
        ] {
            let path = write_mode(dir.path(), "p.toml", &format!("{body}{models}"), 0o600);
            assert_eq!(validate_file(&path), Ok(true), "case {marker}");
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// A `base_url` reqwest cannot parse fails at *request* time as a
    /// transport error, which §6.4 classifies Transient — so a permanent
    /// typo would be retried until the queue dead-lettered the rows, with
    /// nothing naming the cause. It is a load error instead.
    #[test]
    fn an_unusable_base_url_is_refused_at_load_not_retried_forever() {
        for bad in [
            "api.openai.com",
            "ftp://api.openai.com",
            "https://",
            "https://api.openai.com/",
            "https://api.openai.com/v1?key=x",
            // Userinfo: no connector reads it (they all authenticate with a
            // header), so it is a credential written where nothing looks and
            // everything that prints a URL leaks.
            "https://user:pass@api.openai.com",
        ] {
            assert!(validate_base_url(bad).is_err(), "{bad:?}");
        }
        for good in [
            "https://api.openai.com",
            "http://127.0.0.1:8080",
            "https://example.azure.com/openai/deployments/d",
        ] {
            assert!(validate_base_url(good).is_ok(), "{good:?}");
        }

        // Plaintext off-host needs an explicit opt-in; loopback does not.
        assert!(base_url_is_plaintext_offhost("http://vllm.internal:8000"));
        assert!(base_url_is_plaintext_offhost("http://10.0.0.4:8000"));
        assert!(!base_url_is_plaintext_offhost("http://127.0.0.1:8000"));
        assert!(!base_url_is_plaintext_offhost("http://localhost:8000"));
        assert!(!base_url_is_plaintext_offhost("http://[::1]:8000"));
        assert!(!base_url_is_plaintext_offhost("https://api.openai.com"));

        let dir = private_tempdir();
        let path = write_mode(
            dir.path(),
            "openai.toml",
            "provider = \"openai\"\napi_key = \"k\"\nbase_url = \"api.openai.com\"\n\n\
             [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n",
            0o600,
        );
        assert!(validate_file(&path).unwrap_err().contains("base_url"));
        assert!(load_file(&path).is_err());
    }

    /// `providers.d` is operator input to a long-lived process that, in
    /// embedded mode, is the PostgreSQL launcher. Per-file rules bound one
    /// file; these bound the process. Every one of them fails the *whole*
    /// scan rather than half-applying, so a snapshot is never built from a
    /// directory that broke a ceiling.
    #[test]
    fn the_directory_is_bounded_as_a_whole_not_only_per_file() {
        let dir = private_tempdir();
        let file = |models: &str| {
            format!("provider = \"openai\"\napi_key = \"k\"\nmax_concurrent = 64\n{models}")
        };
        let one_model = "\n[[models]]\nname = \"m\"\nprovider_model_id = \"m\"\ndim = 4\n";

        // Aggregate outbound concurrency: both hosts add this sum to their
        // gRPC ingress width, so it is the multiplier on the whole feature's
        // memory footprint.
        for i in 0..8 {
            write_mode(
                dir.path(),
                &format!("p{i}.toml"),
                &file(&one_model.replace("\"m\"", &format!("\"m{i}\""))),
                0o600,
            );
        }
        let refused = load_dir(dir.path()).unwrap_err();
        assert!(refused.contains("total outbound concurrency"), "{refused}");

        // File count.
        let dir = private_tempdir();
        for i in 0..40 {
            write_mode(
                dir.path(),
                &format!("p{i:02}.toml"),
                &format!(
                    "provider = \"openai\"\napi_key = \"k\"\nmax_concurrent = 1{}",
                    one_model.replace("\"m\"", &format!("\"m{i}\""))
                ),
                0o600,
            );
        }
        let refused = load_dir(dir.path()).unwrap_err();
        assert!(refused.contains("provider files"), "{refused}");

        // A single oversized file, and an oversized secret.
        let dir = private_tempdir();
        write_mode(
            dir.path(),
            "big.toml",
            &format!("# {}\nprovider = \"openai\"\n", "x".repeat(300 * 1024)),
            0o600,
        );
        let outcome = load_dir(dir.path()).unwrap();
        assert!(
            outcome.errors[0].message.contains("ceiling"),
            "{:?}",
            outcome.errors
        );
    }

    /// The 0600 rule has to hold for the file that is actually *read*, not
    /// for whatever the path pointed at when it was stat'ed. `O_NOFOLLOW`
    /// makes a symlink a refusal at open, and the mode is checked on the
    /// resulting descriptor.
    #[test]
    fn a_symlinked_provider_file_or_secret_is_refused_at_open() {
        let dir = private_tempdir();
        let elsewhere = private_tempdir();

        // A world-readable key file a symlink tries to launder.
        let real_key = write_mode(elsewhere.path(), "leaky.key", "sk-secret", 0o644);
        let link = dir.path().join("openai.key");
        std::os::unix::fs::symlink(&real_key, &link).unwrap();
        write_mode(
            dir.path(),
            "openai.toml",
            &format!(
                "provider = \"openai\"\napi_key_file = \"{}\"\n\n[[models]]\nname = \"m1\"\n\
                 provider_model_id = \"m\"\ndim = 4\n",
                link.display()
            ),
            0o600,
        );
        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert!(
            outcome.errors[0].message.contains("symlink"),
            "{:?}",
            outcome.errors
        );
        assert!(!outcome.errors[0].message.contains("sk-secret"));

        // And the connector file itself.
        let dir = private_tempdir();
        let real = write_mode(elsewhere.path(), "real.toml", OPENAI_TOML, 0o600);
        std::os::unix::fs::symlink(&real, dir.path().join("openai.toml")).unwrap();
        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.providers.is_empty());
        assert!(
            outcome.errors[0].message.contains("symlink"),
            "{:?}",
            outcome.errors
        );
    }

    /// A group- or world-writable providers.d is not a warning: anyone who
    /// can write there can add a connector file and choose where this host
    /// sends source text. That is strictly worse than the world-readable key
    /// file this module already refuses.
    #[test]
    fn a_writable_providers_directory_is_refused_outright() {
        let dir = private_tempdir();
        write_mode(dir.path(), "openai.toml", OPENAI_TOML, 0o600);
        assert_eq!(load_dir(dir.path()).unwrap().providers.len(), 1);

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let refused = load_dir(dir.path()).unwrap_err();
        assert!(refused.contains("writable by other users"), "{refused}");

        // Read and execute bits only disclose which providers exist, which is
        // doctor's business rather than a refusal.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(load_dir(dir.path()).unwrap().providers.len(), 1);
    }

    /// The flat TOML schema describes a tagged union: `provider` decides
    /// which fields mean anything. `deny_unknown_fields` cannot express that,
    /// so each connector arm rejects the fields it does not consume — an
    /// operator must never be able to believe one credential is in use while
    /// the connector reads another (or none).
    #[test]
    fn fields_the_selected_connector_ignores_are_refused() {
        let dir = private_tempdir();
        let models = "\n\n[[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n";
        for (marker, body, expect) in [
            (
                // The factory silently prefers the bearer token.
                "aws with both credential families",
                format!(
                    "provider = \"aws\"\nregion = \"us-east-1\"\nbearer_token = \"t\"\n\
                     access_key_id = \"AKIA\"\nsecret_access_key = \"s\"{models}"
                ),
                "ignore the pair",
            ),
            (
                "aws with an api_key",
                format!(
                    "provider = \"aws\"\nregion = \"us-east-1\"\nbearer_token = \"t\"\n\
                     api_key = \"sk\"{models}"
                ),
                "does not use api_key",
            ),
            (
                "openai with a region",
                format!("provider = \"openai\"\napi_key = \"k\"\nregion = \"us-east-1\"{models}"),
                "applies only to provider \"aws\"",
            ),
            (
                "openai with a bearer token",
                format!("provider = \"openai\"\napi_key = \"k\"\nbearer_token = \"t\"{models}"),
                "apply only to provider \"aws\"",
            ),
            (
                "a dim pgvector cannot store",
                "provider = \"openai\"\napi_key = \"k\"\n\n[[models]]\nname = \"m1\"\n\
                 provider_model_id = \"m\"\ndim = 20000\n"
                    .to_string(),
                "VECTOR_MAX_DIM",
            ),
            (
                "an unbounded timeout",
                format!("provider = \"openai\"\napi_key = \"k\"\ntimeout_ms = 99999999{models}"),
                "timeout_ms must be between",
            ),
            (
                "a max_tokens of zero",
                "provider = \"openai\"\napi_key = \"k\"\n\n[[models]]\nname = \"m1\"\n\
                 provider_model_id = \"m\"\ndim = 4\nmax_tokens = 0\n"
                    .to_string(),
                "max_tokens must be positive",
            ),
        ] {
            let path = write_mode(dir.path(), "p.toml", &body, 0o600);
            let refused = validate_file(&path).unwrap_err();
            assert!(refused.contains(expect), "case {marker}: {refused}");
            assert!(load_file(&path).is_err(), "case {marker} (loader)");
            std::fs::remove_file(&path).unwrap();
        }
    }

    /// Plaintext to a non-loopback host puts the API key **and every
    /// document the column embeds** on the network in the clear. A
    /// self-hosted OpenAI-compatible endpoint on a private network is a real
    /// deployment, so it stays possible — but only in writing. Loopback (a
    /// sidecar, a mock) needs no ceremony.
    #[test]
    fn plaintext_off_host_needs_an_explicit_opt_in() {
        let dir = private_tempdir();
        let file = |base_url: &str, opt_in: &str| {
            format!(
                "provider = \"openai\"\napi_key = \"k\"\nbase_url = \"{base_url}\"\n{opt_in}\n\
                 [[models]]\nname = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n"
            )
        };

        let path = write_mode(
            dir.path(),
            "p.toml",
            &file("http://vllm.internal:8000", ""),
            0o600,
        );
        let refused = validate_file(&path).unwrap_err();
        assert!(refused.contains("allow_insecure_transport"), "{refused}");
        assert!(load_file(&path).is_err());

        let path = write_mode(
            dir.path(),
            "p.toml",
            &file(
                "http://vllm.internal:8000",
                "allow_insecure_transport = true",
            ),
            0o600,
        );
        assert_eq!(validate_file(&path), Ok(true), "the opt-in is honoured");

        // Loopback is allowed without it.
        let path = write_mode(
            dir.path(),
            "p.toml",
            &file("http://127.0.0.1:8000", ""),
            0o600,
        );
        assert_eq!(validate_file(&path), Ok(true));
    }

    /// The CLI, the embedded launcher and postvec-server all resolve a
    /// referenced secret path, and none of them share a working directory.
    #[test]
    fn a_relative_secret_path_is_refused() {
        let dir = private_tempdir();
        let path = write_mode(
            dir.path(),
            "p.toml",
            "provider = \"openai\"\napi_key_file = \"keys/openai.key\"\n\n[[models]]\n\
             name = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n",
            0o600,
        );
        let refused = validate_file(&path).unwrap_err();
        assert!(refused.contains("absolute path"), "{refused}");
    }

    /// The leaf's own mode says nothing about whether the *path to it* can be
    /// rewritten. `postvec.providers_path` is operator-configurable, so "it
    /// lives under /etc/postvec" is an assumption; a writable ancestor lets
    /// another account swap the directory between validation and enumeration.
    /// The sticky bit is the exception that keeps `/tmp` usable.
    #[test]
    fn a_writable_ancestor_is_refused_unless_it_is_sticky() {
        let root = private_tempdir();
        let parent = root.path().join("open");
        let dir = parent.join("providers.d");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_directory(&dir, None).is_ok(), "read bits are fine");

        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        let refused = validate_directory(&dir, None).unwrap_err();
        assert!(refused.contains("replaced"), "{refused}");
        assert!(refused.contains("open"), "{refused}");

        // Sticky: only an entry's owner may rename it, which is the property
        // being checked for — so `/tmp`-shaped ancestors stay usable.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(validate_directory(&dir, None).is_ok(), "sticky is safe");

        // Ownership, not only permissions. A 0755 ancestor owned by another
        // account is writable **by that account**, so its owner can replace
        // the component below it whatever the mode says — which is also what
        // makes a sticky parent safe only when the child is ours. Exercised
        // through `validate_ancestry` directly, because the leaf's own owner
        // check would otherwise fire first.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mine = unsafe { libc::geteuid() };
        assert!(validate_ancestry(&dir, mine).is_ok());
        let refused = validate_ancestry(&dir, mine.wrapping_add(4242)).unwrap_err();
        assert!(refused.contains("owned by uid"), "{refused}");
        assert!(refused.contains("replace the path"), "{refused}");
    }

    /// Google's embedding generations differ in whether they take `taskType`
    /// and `outputDimensionality`, and in whether retrieval intent is a
    /// prompt prefix instead. Serving a model whose contract is unknown would
    /// embed queries and documents identically while the descriptor claims
    /// the distinction is honoured — a silent, permanent quality loss in
    /// `search()`. So it is refused, not degraded.
    #[test]
    fn an_unknown_gemini_model_is_refused_rather_than_served_without_its_semantics() {
        let dir = private_tempdir();
        let file = |id: &str, dim: u32| {
            format!(
                "provider = \"google\"\napi_key = \"k\"\n\n[[models]]\n\
                 name = \"m1\"\nprovider_model_id = \"{id}\"\ndim = {dim}\n"
            )
        };
        let path = write_mode(
            dir.path(),
            "p.toml",
            &file("gemini-embedding-001", 3072),
            0o600,
        );
        assert_eq!(validate_file(&path), Ok(true));

        // A reduced-but-legal width is accepted; the documented range is
        // 128..=3072 and the client asks for it explicitly.
        let path = write_mode(
            dir.path(),
            "p.toml",
            &file("gemini-embedding-001", 768),
            0o600,
        );
        assert_eq!(validate_file(&path), Ok(true));

        // Outside that range: refused, rather than 400-ing on every call.
        for dim in [4, 127, 3073] {
            let path = write_mode(
                dir.path(),
                "p.toml",
                &file("gemini-embedding-001", dim),
                0o600,
            );
            let refused = validate_file(&path).unwrap_err();
            assert!(refused.contains("128..=3072"), "dim {dim}: {refused}");
        }

        // An id this crate has no contract for — including Google's retired
        // ones, which an earlier pass listed without checking their
        // lifecycle. An allow-list of dead models is worse than none: it
        // reads as verification.
        for id in ["gemini-embedding-2", "text-embedding-004", "embedding-001"] {
            let path = write_mode(dir.path(), "p.toml", &file(id, 768), 0o600);
            let refused = validate_file(&path).unwrap_err();
            assert!(refused.contains("request contract"), "{id}: {refused}");
            assert!(refused.contains("gemini-embedding-001"), "{id}: {refused}");
            assert!(load_file(&path).is_err(), "{id}");
        }
    }

    /// The loader and the client must read the *same* table: a second list is
    /// how a validated descriptor ends up sending a field the model rejects.
    #[test]
    fn the_gemini_contract_has_exactly_one_source() {
        for contract in crate::gemini::GEMINI_CONTRACTS {
            assert!(
                crate::gemini::gemini_contract(contract.model_id).is_some(),
                "{} must be reachable through the lookup the client uses",
                contract.model_id
            );
            if let Some((low, high)) = contract.dims {
                assert!(low > 0 && low <= high, "{}", contract.model_id);
            }
        }
        // Retired models are absent, not listed.
        assert!(crate::gemini::gemini_contract("text-embedding-004").is_none());
        assert!(crate::gemini::gemini_contract("embedding-001").is_none());
    }

    /// The placeholder exists so the *other* structural rules can run before
    /// the probe. It is only sound if it satisfies the per-model rules for
    /// every provider — otherwise pre-probe validation would refuse a file
    /// that is actually fine.
    #[test]
    fn every_placeholder_dimension_satisfies_its_own_rule() {
        let cases = [
            ("openai", "text-embedding-3-small"),
            ("openai", "some-future-model"),
            ("openrouter", "openai/text-embedding-3-large"),
            ("mistral", "mistral-embed"),
            ("cohere", "embed-v4.0"),
            ("cohere", "embed-english-v3.0"),
            ("cohere", "embed-multilingual-light-v3.0"),
            ("cohere", "embed-v4.5-hypothetical"),
            ("google", "gemini-embedding-001"),
            ("aws", "amazon.titan-embed-text-v2:0"),
        ];
        for (provider, id) in cases {
            let dim = placeholder_dim(provider, id);
            assert!(
                validate_model_for_provider(provider, id, Some(dim)).is_ok(),
                "{provider}/{id}: placeholder {dim} is refused by the rule it exists to pass"
            );
            assert!(dim > 0 && dim <= MAX_DIM, "{provider}/{id}: {dim}");
        }
    }

    /// Cohere v4 produces exactly four widths and each v3 model exactly one.
    /// A descriptor asking for another is a 400 (v4) or a dimension mismatch
    /// on every response (v3), so the file is refused rather than the first
    /// insert.
    #[test]
    fn a_cohere_dimension_the_model_cannot_produce_is_refused() {
        let dir = private_tempdir();
        let file = |id: &str, dim: u32| {
            format!(
                "provider = \"cohere\"\napi_key = \"k\"\n\n[[models]]\n\
                 name = \"m1\"\nprovider_model_id = \"{id}\"\ndim = {dim}\n"
            )
        };
        for dim in [256, 512, 1024, 1536] {
            let path = write_mode(dir.path(), "p.toml", &file("embed-v4.0", dim), 0o600);
            assert_eq!(validate_file(&path), Ok(true), "v4 accepts {dim}");
        }
        let path = write_mode(dir.path(), "p.toml", &file("embed-v4.0", 768), 0o600);
        let refused = validate_file(&path).unwrap_err();
        assert!(refused.contains("Cohere v4 produces"), "{refused}");

        // v3 widths are fixed per model, so they are checked *exactly*
        // rather than against a set — a descriptor claiming another number
        // would fail the gateway's dimension check on every response.
        for (id, dim) in COHERE_FIXED_DIMS {
            let path = write_mode(dir.path(), "p.toml", &file(id, *dim), 0o600);
            assert_eq!(validate_file(&path), Ok(true), "{id} at its real width");

            let path = write_mode(dir.path(), "p.toml", &file(id, dim + 1), 0o600);
            let refused = validate_file(&path).unwrap_err();
            assert!(
                refused.contains(&format!("always returns {dim}")),
                "{id}: {refused}"
            );
            assert!(load_file(&path).is_err(), "{id}");
        }
        // Explicitly including the light models, whose 384 is the width most
        // likely to be assumed to be 1024 like its siblings.
        assert!(COHERE_FIXED_DIMS
            .iter()
            .any(|(id, dim)| *id == "embed-english-light-v3.0" && *dim == 384));
        assert!(COHERE_FIXED_DIMS
            .iter()
            .any(|(id, dim)| *id == "embed-multilingual-light-v3.0" && *dim == 384));
    }

    /// Mode alone is not a trust boundary when the reader is root: a 0700
    /// directory owned by another account passes every permission test, and
    /// root reads straight through it.
    #[test]
    fn a_directory_owned_by_someone_else_is_refused() {
        let dir = private_tempdir();
        let mine = unsafe { libc::geteuid() };
        assert!(validate_directory(dir.path(), None).is_ok());
        assert!(validate_directory(dir.path(), Some(mine)).is_ok());
        // Root-owned is always acceptable; a third identity is not.
        let refused = validate_directory(dir.path(), Some(mine.wrapping_add(4242))).unwrap_err();
        assert!(refused.contains("owned by uid"), "{refused}");
    }

    #[test]
    fn implausible_descriptors_are_refused() {
        let dir = private_tempdir();
        for (marker, body) in [
            (
                "dim",
                "provider = \"openai\"\napi_key = \"k\"\n\n[[models]]\nname = \"m1\"\n\
                 provider_model_id = \"m\"\ndim = 0\n",
            ),
            (
                "model name",
                "provider = \"openai\"\napi_key = \"k\"\n\n[[models]]\nname = \"Bad Name\"\n\
                 provider_model_id = \"m\"\ndim = 4\n",
            ),
            (
                "max_concurrent",
                "provider = \"openai\"\napi_key = \"k\"\nmax_concurrent = 0\n\n[[models]]\n\
                 name = \"m1\"\nprovider_model_id = \"m\"\ndim = 4\n",
            ),
            ("models", "provider = \"openai\"\napi_key = \"k\"\n"),
        ] {
            let file = write_mode(dir.path(), "p.toml", body, 0o600);
            let outcome = load_dir(dir.path()).unwrap();
            assert_eq!(outcome.errors.len(), 1, "case {marker}");
            std::fs::remove_file(file).unwrap();
        }
    }

    /// A complete UniVec converter entry loads, with every field where the
    /// gateway and the route identity expect it.
    #[test]
    fn a_univec_converter_entry_loads_with_both_vocabularies() {
        let dir = private_tempdir();
        write_mode(
            dir.path(),
            "univec.toml",
            "provider = \"univec\"\napi_key = \"uv\"\n\n\
             [[models]]\nname = \"univec-convert-a-to-b\"\nkind = \"convert\"\n\
             provider_model_id = \"target-space\"\nprovider_source_id = \"source-space\"\n\
             source_model = \"model-a\"\ntarget_model = \"model-b\"\n\
             source_dim = 1536\ndim = 768\n",
            0o600,
        );
        let outcome = load_dir(dir.path()).unwrap();
        assert!(outcome.errors.is_empty(), "{:?}", outcome.errors);
        let model = &outcome.providers[0].models[0];
        assert_eq!(model.kind, ModelKind::Convert);
        assert_eq!(model.provider_source_id.as_deref(), Some("source-space"));
        assert_eq!(model.source_model.as_deref(), Some("model-a"));
        assert_eq!(model.target_model.as_deref(), Some("model-b"));
        assert_eq!(model.source_dim, Some(1536));
        assert_eq!(model.dim, 768);
        assert_eq!(
            model.route_model_id(),
            "source-space->target-space for model-a[1536]->model-b"
        );
    }

    /// The converter tagged-union rules, one refusal apiece. Every case is a
    /// whole-file load error — same blast radius as any other schema fault.
    #[test]
    fn converter_entries_are_held_to_their_own_rules() {
        let dir = private_tempdir();
        let convert_entry = "[[models]]\nname = \"c\"\nkind = \"convert\"\n\
             provider_model_id = \"t\"\nprovider_source_id = \"s\"\n\
             source_model = \"model-a\"\ntarget_model = \"model-b\"\n\
             source_dim = 4\ndim = 4\n";
        for (marker, body) in [
            (
                "convert under a connector with no conversion endpoint",
                format!("provider = \"openai\"\napi_key = \"k\"\n\n{convert_entry}"),
            ),
            (
                "missing provider_source_id",
                "provider = \"univec\"\napi_key = \"k\"\n\n[[models]]\nname = \"c\"\n\
                 kind = \"convert\"\nprovider_model_id = \"t\"\nsource_model = \"model-a\"\n\
                 target_model = \"model-b\"\nsource_dim = 4\ndim = 4\n"
                    .to_string(),
            ),
            (
                "missing source_dim",
                "provider = \"univec\"\napi_key = \"k\"\n\n[[models]]\nname = \"c\"\n\
                 kind = \"convert\"\nprovider_model_id = \"t\"\nprovider_source_id = \"s\"\n\
                 source_model = \"model-a\"\ntarget_model = \"model-b\"\ndim = 4\n"
                    .to_string(),
            ),
            (
                "a converter between a space and itself",
                "provider = \"univec\"\napi_key = \"k\"\n\n[[models]]\nname = \"c\"\n\
                 kind = \"convert\"\nprovider_model_id = \"t\"\nprovider_source_id = \"s\"\n\
                 source_model = \"model-a\"\ntarget_model = \"model-a\"\nsource_dim = 4\ndim = 4\n"
                    .to_string(),
            ),
            (
                "max_tokens on a converter",
                "provider = \"univec\"\napi_key = \"k\"\n\n[[models]]\nname = \"c\"\n\
                 kind = \"convert\"\nprovider_model_id = \"t\"\nprovider_source_id = \"s\"\n\
                 source_model = \"model-a\"\ntarget_model = \"model-b\"\nsource_dim = 4\ndim = 4\n\
                 max_tokens = 8192\n"
                    .to_string(),
            ),
            (
                "converter fields on an embed entry",
                "provider = \"univec\"\napi_key = \"k\"\n\n[[models]]\nname = \"e\"\n\
                 provider_model_id = \"m\"\ndim = 4\nsource_dim = 4\n"
                    .to_string(),
            ),
        ] {
            let file = write_mode(dir.path(), "p.toml", &body, 0o600);
            let outcome = load_dir(dir.path()).unwrap();
            assert_eq!(outcome.errors.len(), 1, "case: {marker}");
            assert!(outcome.providers.is_empty(), "case: {marker}");
            std::fs::remove_file(file).unwrap();
        }
    }

    /// Route identity for a converter folds in everything that decides what
    /// the route means. A same-name change of the postvec-side source space
    /// — which redirects OTHER columns' migrations through this converter —
    /// must compare unequal, exactly like an embed model's id or dim change.
    #[test]
    fn a_converters_route_identity_covers_both_vocabularies() {
        let dir = private_tempdir();
        let body = |source_model: &str| {
            format!(
                "provider = \"univec\"\napi_key = \"uv\"\n\n\
                 [[models]]\nname = \"univec-convert\"\nkind = \"convert\"\n\
                 provider_model_id = \"t\"\nprovider_source_id = \"s\"\n\
                 source_model = \"{source_model}\"\ntarget_model = \"model-b\"\n\
                 source_dim = 4\ndim = 4\n"
            )
        };
        let path = dir.path().join("univec.toml");
        let before = served_names_if(dir.path(), &path, Some(&body("model-a"))).unwrap();
        let after = served_names_if(dir.path(), &path, Some(&body("model-c"))).unwrap();
        let before = before.get("univec-convert").unwrap();
        let after = after.get("univec-convert").unwrap();
        assert_ne!(
            before.model_id, after.model_id,
            "a source-space change must read as a different route"
        );
        assert_eq!(before.dim, 4, "dim stays the OUTPUT dimension");
    }
}
