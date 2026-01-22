//! The registry index: schema, hard validation, and the shared rules.
//!
//! One JSON document per channel. The rules enforced here are the client's
//! whole trust decision short of the archive digest itself, so parsing is
//! strict where the spec is strict (unknown `schema_version` is fatal, names
//! are bounded and unique, `access` must be stated, the dependency graph must
//! be a complete DAG) and tolerant where it promises tolerance (unknown
//! fields are ignored).
//!
//! This crate is the **single implementation** (registry-simplify.md §4.6):
//! the CLI validates what it downloads, the publisher validates what it is
//! about to write, and the aphex gateway validates what it is about to serve
//! — all through [`parse_and_validate`]. The three cannot drift because they
//! are the same code.
//!
//! The same reasoning brings three more shared pieces here, each written
//! once and read by both the client and the publisher:
//!
//! - [`descriptor`]: the engine descriptor a published archive carries, and
//!   its agreement with the index entry that advertises it;
//! - [`identity`]: what a model name promises across revisions;
//! - [`archive`]: deterministic tar writing and the strict, bounded reader
//!   (behind the `archive` feature, so a gateway that only validates an
//!   index does not compile a tar implementation it never calls).

pub mod descriptor;
pub mod identity;

#[cfg(feature = "archive")]
pub mod archive;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// The one schema version this implementation understands.
pub const SCHEMA_VERSION: u32 = 1;

/// The index body is capped before parsing.
pub const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;

/// Bounds promised by the schema rules ("names/lists bounded").
pub const MAX_MODELS: usize = 4096;
pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_LIST_ITEMS: usize = 64;
pub const MAX_SOURCES: usize = 8;

/// Sanity ceiling for a single archive; nothing in the catalogue is remotely
/// close, so anything larger is a corrupt or hostile index.
///
/// The value is the largest size a classic ustar 12-byte octal size field can
/// encode (2^33 − 1 ≈ 8 GiB): the deterministic writer emits plain ustar and
/// the strict reader deliberately understands nothing bigger, so advertising
/// more than the format can represent would just move the failure later.
pub const MAX_ARCHIVE_BYTES: u64 = 0o77777777777;

/// Model types the registry may list. `pull` plans downloads and companions
/// from this field, so an unknown type is a malformed index, not a tolerated
/// extension (no guessed load-bearing defaults).
pub const KNOWN_MODEL_TYPES: &[&str] = &["embed", "convert", "embed-bridge", "convert-bridge"];

/// Entitlement ids this implementation understands
/// (registry-public-first-party.md §2). One list for the publisher's
/// refusal and the gateway's filter — the crate exists for exactly this
/// reason. Strict validation refuses an id outside it; the gateway treats
/// such an id as one the caller does not hold, so an aphex older than a
/// catalogue restriction hides the entry rather than presigning it. Both
/// directions fail closed.
pub const KNOWN_ENTITLEMENTS: &[&str] = &["postvec-catalog"];

/// Bound on one entitlement id; nothing real approaches it.
pub const MAX_ENTITLEMENT_ID_BYTES: usize = 64;

/// Acceptance policies a terms document may carry (M2). `none` needs no
/// acknowledgement, `notice` requires the CLI to show the exact document and
/// record a local acknowledgement before download, and `organization` is
/// reserved for M4b's server-side acceptance gate — representable at schema
/// v1 so the wire format never needs to move, but not publishable or
/// servable until that gate exists.
pub const KNOWN_LICENSE_ACCEPTANCE: &[&str] = &["none", "notice", "organization"];

/// Bound on a licence id. Same as [`MAX_NAME_BYTES`], and the id shares the
/// model-name grammar, because it is the first half of the
/// `--accept-license <id>@<version>` CLI token.
pub const MAX_LICENSE_ID_BYTES: usize = 128;

/// Bound on a terms-document version string. It doubles as half of the
/// `--accept-license <id>@<version>` CLI token, so it is validated much
/// harder than its length.
pub const MAX_LICENSE_VERSION_BYTES: usize = 64;

/// Bound on a terms-document canonical URL.
pub const MAX_LICENSE_URL_BYTES: usize = 512;

/// Bound on a viewer entitlement's `expires_at` timestamp string.
pub const MAX_EXPIRES_AT_BYTES: usize = 64;

/// Sanity ceiling on a name's revision counter, so a hostile or corrupt index
/// cannot express an unreachable number. Nothing real approaches it.
pub const MAX_REVISION: u64 = 1_000_000;

/// Why an index was rejected, at the granularity callers react to.
#[derive(Debug)]
pub enum SchemaError {
    /// Body exceeds [`MAX_INDEX_BYTES`].
    TooLarge(usize),
    /// Not valid JSON, or does not deserialize as schema v1.
    Json(String),
    /// A `schema_version` this implementation does not understand — the one
    /// case whose remedy is "upgrade the client", so it is distinguishable.
    SchemaVersion(u32),
    /// A structural rule failed.
    Invalid(String),
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaError::TooLarge(actual) => write!(
                f,
                "registry index is {actual} bytes; the limit is {MAX_INDEX_BYTES}"
            ),
            SchemaError::Json(detail) => write!(f, "registry index does not parse: {detail}"),
            SchemaError::SchemaVersion(version) => write!(
                f,
                "registry index has schema_version {version}, this build understands {SCHEMA_VERSION}"
            ),
            SchemaError::Invalid(detail) => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for SchemaError {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    pub schema_version: u32,
    pub channel: String,
    #[serde(default)]
    pub authenticated: bool,
    #[serde(default)]
    pub generated_at: Option<String>,
    pub models: Vec<IndexModel>,
    /// Caller-specific metadata the gateway stamps onto an authenticated
    /// *response*: the entitlements this caller holds, each with the source
    /// that granted it. Permitted only on authenticated documents and always
    /// refused on public/anonymous ones. The publisher never writes it, and
    /// aphex refuses it in any *stored* index (only aphex knows a document
    /// is stored rather than served). Carries no PII, billing data, grant
    /// identifier, contract reference, price, email, organization name or
    /// URL — the index is not where a customer learns those.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer: Option<Viewer>,
}

/// The bounded caller-metadata block on an authenticated response (§4.4).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Viewer {
    pub entitlements: Vec<ViewerEntitlement>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerEntitlement {
    pub id: String,
    /// `"default"` — the deployment grants it to every authenticated caller
    /// via configuration — or `"grant"` (a per-account grant, M4a). One
    /// string that keeps `postvec whoami` truthful: a user who reads
    /// "postvec-catalog (default)" has not been told they bought something.
    pub source: String,
    /// RFC 3339 expiry when the entitlement is time-bounded; `null` for the
    /// configured default. The only timestamp the block may carry.
    #[serde(default)]
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexModel {
    pub name: String,
    /// `public` or `private`. Deliberately **not defaulted**: the gateway
    /// serves a catalogue that mixes public and private records, and
    /// defaulting a missing access class would be the unsafe direction.
    pub access: String,
    pub model_type: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_dim: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dim: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence_len: Option<u32>,
    /// The reviewed terms id. When present it must satisfy
    /// [`valid_license_id`] — the model-name grammar — because it is the
    /// first half of the `--accept-license <id>@<version>` CLI token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The exact version of the terms document (M2), present only when the
    /// publisher's terms policy names one. Present and absent together with
    /// [`IndexModel::license_url`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_version: Option<String>,
    /// Canonical HTTPS URL of that exact document version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_url: Option<String>,
    /// `"none"` | `"notice"` | `"organization"`; **absent means `"none"`**,
    /// so every entry published before versioned terms existed keeps its
    /// behaviour without republication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license_acceptance: Option<String>,
    /// Free-text provenance: where these bytes came from
    /// (registry-simplify.md §4.5) — the first question in six months.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub postvec_requires: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_postvec_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval: Option<serde_json::Value>,
    /// A withdrawn name stays resolvable for installed copies but is
    /// reported by `model ls --available`; it is excluded from new pulls.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub withdrawn: bool,
    /// This name's revision counter: 1 for a first publication, +1 for each
    /// in-place update. Derived by the publisher, never authored and never
    /// selectable by a client — a client installs the head or keeps what it
    /// has. **Absent means 1**, so every entry published before updatable
    /// names existed is revision 1 without republication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Entitlements a caller must hold to receive this entry. Empty (the
    /// default) means any valid authenticated account — what `private`
    /// means today. Never a plan name, an ordering or a price. Additive at
    /// schema v1: aphex filters before serving, so no client is ever handed
    /// an entry it is not entitled to and none needs to understand the
    /// field. A public entry's list must be empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_entitlements: Vec<String>,
    pub archive: ArchiveInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveInfo {
    /// `sha256:<64 lowercase hex>`.
    pub digest: String,
    /// Exact HTTP entity size of the archive.
    pub size: u64,
    /// Exact sum of regular-file payload bytes after extraction.
    pub installed_size: u64,
    /// Ordered download sources; all HTTPS in a real index.
    pub sources: Vec<String>,
}

impl ArchiveInfo {
    /// The digest's hex half, validated to 64 lowercase hex characters.
    pub fn digest_hex(&self) -> Result<&str, String> {
        parse_sha256_digest(&self.digest)
            .ok_or_else(|| format!("digest {:?} is not sha256:<64 lowercase hex>", self.digest))
    }
}

impl IndexModel {
    pub fn is_public(&self) -> bool {
        self.access == "public"
    }

    /// The effective revision: an absent field is revision 1.
    pub fn revision(&self) -> u64 {
        self.revision.unwrap_or(1)
    }

    /// The effective acceptance policy: an absent field is `"none"`.
    pub fn license_acceptance(&self) -> &str {
        self.license_acceptance.as_deref().unwrap_or("none")
    }
}

impl Index {
    pub fn model(&self, name: &str) -> Option<&IndexModel> {
        self.models.iter().find(|m| m.name == name)
    }
}

/// Extract the 64-lowercase-hex half of an `sha256:<hex>` digest. Anything
/// else — wrong prefix, wrong length, uppercase, non-hex — is `None`; a
/// malformed digest must never become a filesystem path or an S3 key.
pub fn parse_sha256_digest(digest: &str) -> Option<&str> {
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        Some(hex)
    } else {
        None
    }
}

/// A registry model name doubles as a directory component and a tar path
/// prefix, so it is validated much harder than a free-form model name:
/// lowercase alphanumerics, `.`, `_`, `-`; must start with an alphanumeric;
/// bounded length. (Uppercase is excluded on purpose — the hub convention is
/// lowercase, and case-insensitive filesystems make `Foo`/`foo` collide.)
pub fn valid_model_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("model name is empty".into());
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(format!("model name exceeds {MAX_NAME_BYTES} bytes"));
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(format!(
            "model name {name:?} must start with a lowercase letter or digit"
        ));
    }
    if let Some(bad) = bytes
        .iter()
        .find(|b| !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "model name {name:?} contains forbidden byte {:?}",
            char::from(*bad)
        ));
    }
    Ok(())
}

/// An entitlement id is an enumerated policy token, validated harder than a
/// free-form string: lowercase alphanumerics and `-`, starting alphanumeric,
/// bounded. (Membership in [`KNOWN_ENTITLEMENTS`] is a separate, strictness-
/// dependent rule — shape is checked in every mode.)
pub fn valid_entitlement_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("entitlement id is empty".into());
    }
    if id.len() > MAX_ENTITLEMENT_ID_BYTES {
        return Err(format!(
            "entitlement id exceeds {MAX_ENTITLEMENT_ID_BYTES} bytes"
        ));
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(format!(
            "entitlement id {id:?} must start with a lowercase letter or digit"
        ));
    }
    if let Some(bad) = bytes
        .iter()
        .find(|b| !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-'))
    {
        return Err(format!(
            "entitlement id {id:?} contains forbidden byte {:?}",
            char::from(*bad)
        ));
    }
    Ok(())
}

/// A licence id doubles as the first half of the
/// `--accept-license <id>@<version>` CLI token, so it is held to the same
/// grammar as a model name: lowercase alphanumerics plus `.`, `_`, `-`,
/// starting with an alphanumeric, bounded by [`MAX_LICENSE_ID_BYTES`]. The
/// rule is what makes every schema-valid entry *acknowledgeable* — an index
/// can never carry a notice document whose id the CLI cannot express as a
/// flag.
pub fn valid_license_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("license id is empty".into());
    }
    if id.len() > MAX_LICENSE_ID_BYTES {
        return Err(format!("license id exceeds {MAX_LICENSE_ID_BYTES} bytes"));
    }
    let bytes = id.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(format!(
            "license id {id:?} must start with a lowercase letter or digit"
        ));
    }
    if let Some(bad) = bytes
        .iter()
        .find(|b| !matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "license id {id:?} contains forbidden byte {:?} (it must be usable as an \
             `--accept-license <id>@<version>` token)",
            char::from(*bad)
        ));
    }
    Ok(())
}

/// A terms-document version doubles as half of the
/// `--accept-license <id>@<version>` CLI token, so beyond being bounded it
/// may contain no whitespace, no control characters and no `@` — printable
/// ASCII only. Versions like `2026-08-09` or `1.2` pass; anything that
/// could split, hide inside, or confuse the token fails.
pub fn valid_license_version(version: &str) -> Result<(), String> {
    if version.is_empty() {
        return Err("license_version is empty".into());
    }
    if version.len() > MAX_LICENSE_VERSION_BYTES {
        return Err(format!(
            "license_version exceeds {MAX_LICENSE_VERSION_BYTES} bytes"
        ));
    }
    if let Some(bad) = version
        .bytes()
        .find(|b| !b.is_ascii_graphic() || *b == b'@')
    {
        return Err(format!(
            "license_version {version:?} contains forbidden byte {:?} (printable ASCII \
             without whitespace or `@` only — it must be usable as an \
             `--accept-license <id>@<version>` token)",
            char::from(bad)
        ));
    }
    Ok(())
}

/// A terms-document URL must be a bounded, absolute HTTPS reference with a
/// host and no embedded credentials. Validated by hand so this crate stays
/// serde-only: the rule is "safe to print and follow", not full URL
/// semantics.
pub fn valid_license_url(url: &str) -> Result<(), String> {
    if url.len() > MAX_LICENSE_URL_BYTES {
        return Err(format!("license_url exceeds {MAX_LICENSE_URL_BYTES} bytes"));
    }
    if let Some(bad) = url.bytes().find(|b| !b.is_ascii_graphic()) {
        return Err(format!(
            "license_url {url:?} contains forbidden byte {:?}",
            char::from(bad)
        ));
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!(
            "license_url {url:?} is not an absolute https:// URL"
        ));
    };
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .expect("split yields at least one part");
    if authority.contains('@') {
        return Err(format!(
            "license_url {url:?} embeds credentials, which a canonical document URL never \
             carries"
        ));
    }
    // The host is the authority minus any `:port` — `https://:443/x` has an
    // authority but no host, and the promise here is a host.
    let host = authority.split(':').next().expect("split yields one part");
    if host.is_empty() {
        return Err(format!("license_url {url:?} has no host"));
    }
    Ok(())
}

/// Whether a `min_postvec_version` value parses as a version. Tolerates the
/// suffixes PostgreSQL packaging appends (`1.2.3-1.pgdg`, `1.2`); rejects
/// anything without a leading numeric major.
pub fn version_parses(raw: &str) -> bool {
    let core = raw.trim();
    let Some(core) = core.split(['-', '+']).next() else {
        return false;
    };
    let mut parts = core
        .split('.')
        .map(|p| p.trim_start_matches(|c: char| !c.is_ascii_digit()))
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        });
    matches!(parts.next(), Some(major) if major.parse::<u64>().is_ok())
}

/// Parse and fully validate an index body.
///
/// The `schema_version` probe runs first on its own so an index from a newer
/// registry produces the one actionable outcome
/// ([`SchemaError::SchemaVersion`]) instead of a field-level parse error.
pub fn parse_and_validate(body: &[u8]) -> Result<Index, SchemaError> {
    parse_and_validate_with(body, EntitlementIdPolicy::Strict)
}

/// [`parse_and_validate`] in the gateway-base mode: every structural,
/// bounds, channel and dependency rule applies unchanged, and only the
/// "entitlement id must be known" rule is relaxed. The aphex gateway admits
/// a *stored* index through this path so a catalogue restriction published
/// by a newer publisher does not 503 the whole route; it then treats every
/// unknown id as one the caller does not hold (the entry is filtered out,
/// never presigned) and strictly revalidates the filtered result before
/// serving it. No other consumer may use this mode.
pub fn parse_and_validate_gateway_base(body: &[u8]) -> Result<Index, SchemaError> {
    parse_and_validate_with(body, EntitlementIdPolicy::AcceptUnknown)
}

fn parse_and_validate_with(body: &[u8], policy: EntitlementIdPolicy) -> Result<Index, SchemaError> {
    if body.len() > MAX_INDEX_BYTES {
        return Err(SchemaError::TooLarge(body.len()));
    }

    #[derive(Deserialize)]
    struct VersionProbe {
        schema_version: u32,
    }
    let probe: VersionProbe =
        serde_json::from_slice(body).map_err(|e| SchemaError::Json(e.to_string()))?;
    if probe.schema_version != SCHEMA_VERSION {
        return Err(SchemaError::SchemaVersion(probe.schema_version));
    }

    let index: Index =
        serde_json::from_slice(body).map_err(|e| SchemaError::Json(e.to_string()))?;
    validate_with(&index, policy)?;
    Ok(index)
}

/// How `required_entitlements` values outside [`KNOWN_ENTITLEMENTS`] are
/// treated. There is exactly one validator; this is the single, narrowly
/// scoped rule the gateway-base mode relaxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntitlementIdPolicy {
    /// Publisher and client: an unknown id is refused outright — a typo
    /// must never enter a catalogue, and a client should not trust a
    /// document its own build cannot fully interpret.
    Strict,
    /// Gateway base index only: an unknown id passes shape validation and
    /// is left for the filter to treat as unheld.
    AcceptUnknown,
}

/// The full rule set, applied to an already-deserialized index. Used by the
/// publisher on the indexes it is about to write, by the client on what it
/// downloads, and by the gateway on the filtered document it is about to
/// serve.
pub fn validate(index: &Index) -> Result<(), SchemaError> {
    validate_with(index, EntitlementIdPolicy::Strict)
}

/// [`validate`] in the gateway-base mode — see
/// [`parse_and_validate_gateway_base`] for the one rule it relaxes.
pub fn validate_gateway_base(index: &Index) -> Result<(), SchemaError> {
    validate_with(index, EntitlementIdPolicy::AcceptUnknown)
}

fn validate_with(index: &Index, policy: EntitlementIdPolicy) -> Result<(), SchemaError> {
    let fail = |detail: String| Err(SchemaError::Invalid(detail));

    if index.models.len() > MAX_MODELS {
        return fail(format!(
            "registry index lists {} models; the limit is {MAX_MODELS}",
            index.models.len()
        ));
    }
    // Channel ↔ authentication ↔ access containment: a public document is
    // anonymous and may only list public entries (a private entry in an
    // anonymously served index would be a disclosure); a private document is
    // the full authenticated catalogue and must say so.
    match index.channel.as_str() {
        "public" => {
            if index.authenticated {
                return fail(
                    "a public-channel index must not claim authenticated: true".to_string(),
                );
            }
        }
        "private" => {
            if !index.authenticated {
                return fail("a private-channel index must set authenticated: true".to_string());
            }
        }
        other => {
            return fail(format!("registry index has unknown channel {other:?}"));
        }
    }

    /// One document identity: keyed by (license id, version), agreeing on
    /// (URL, effective acceptance).
    type TermsDocuments<'a> = BTreeMap<(&'a str, Option<&'a str>), (Option<&'a str>, &'a str)>;

    let mut by_name: BTreeMap<&str, &IndexModel> = BTreeMap::new();
    // One licence id + document version must identify ONE document: within a
    // single index, two entries naming the same (license, license_version)
    // may not disagree on the URL or the acceptance policy.
    let mut terms_documents: TermsDocuments = BTreeMap::new();
    for model in &index.models {
        valid_model_name(&model.name).map_err(SchemaError::Invalid)?;
        if by_name.insert(model.name.as_str(), model).is_some() {
            return fail(format!(
                "registry index lists {:?} more than once",
                model.name
            ));
        }
        if !matches!(model.access.as_str(), "public" | "private") {
            return fail(format!(
                "{}: unknown access class {:?}",
                model.name, model.access
            ));
        }
        if index.channel == "public" && model.access != "public" {
            return fail(format!(
                "{}: a public-channel index may not list a private entry",
                model.name
            ));
        }
        valid_model_name(&model.backend)
            .map_err(|e| SchemaError::Invalid(format!("{}: backend: {e}", model.name)))?;
        if !KNOWN_MODEL_TYPES.contains(&model.model_type.as_str()) {
            return fail(format!(
                "{}: unknown model_type {:?} (this build understands {})",
                model.name,
                model.model_type,
                KNOWN_MODEL_TYPES.join(", ")
            ));
        }
        for (label, dim) in [
            ("source_dim", model.source_dim),
            ("target_dim", model.target_dim),
            ("sequence_len", model.sequence_len),
        ] {
            if dim == Some(0) {
                return fail(format!("{}: {label} is zero", model.name));
            }
        }
        // Revisions are counted from 1 and bounded; 0 would be an off-by-one
        // that reads as "older than every install", and an absurd value could
        // only come from corruption.
        if let Some(revision) = model.revision {
            if revision == 0 || revision > MAX_REVISION {
                return fail(format!(
                    "{}: revision {revision} is outside 1..={MAX_REVISION}",
                    model.name
                ));
            }
        }
        // A malformed minimum version must fail the index, never be silently
        // discarded — a typo here would otherwise disable a compatibility
        // gate.
        if let Some(minimum) = &model.min_postvec_version {
            if !version_parses(minimum) {
                return fail(format!(
                    "{}: min_postvec_version {minimum:?} is not a valid version",
                    model.name
                ));
            }
        }
        // Versioned terms metadata (M2). All additive at schema v1: absent
        // fields read exactly as the pre-M2 behaviour (`none`, unversioned).
        // The licence id itself is held to the token grammar whenever
        // present, so every valid entry is expressible as an
        // `--accept-license <id>@<version>` acknowledgement.
        if let Some(license) = &model.license {
            valid_license_id(license)
                .map_err(|e| SchemaError::Invalid(format!("{}: {e}", model.name)))?;
        }
        let acceptance = model.license_acceptance();
        if !KNOWN_LICENSE_ACCEPTANCE.contains(&acceptance) {
            return fail(format!(
                "{}: unknown license_acceptance {acceptance:?} (this build understands {})",
                model.name,
                KNOWN_LICENSE_ACCEPTANCE.join(", ")
            ));
        }
        if model.license.is_none()
            && (model.license_version.is_some()
                || model.license_url.is_some()
                || model.license_acceptance.is_some())
        {
            return fail(format!(
                "{}: terms metadata (license_version/license_url/license_acceptance) without \
                 a license id",
                model.name
            ));
        }
        match (&model.license_version, &model.license_url) {
            (Some(version), Some(url)) => {
                valid_license_version(version)
                    .map_err(|e| SchemaError::Invalid(format!("{}: {e}", model.name)))?;
                valid_license_url(url)
                    .map_err(|e| SchemaError::Invalid(format!("{}: {e}", model.name)))?;
            }
            (None, None) => {
                // An unversioned document cannot demand an acknowledgement:
                // there is nothing exact to acknowledge.
                if acceptance != "none" {
                    return fail(format!(
                        "{}: license_acceptance {acceptance:?} requires a license_version and \
                         a canonical HTTPS license_url",
                        model.name
                    ));
                }
            }
            _ => {
                return fail(format!(
                    "{}: license_version and license_url must be both present or both absent",
                    model.name
                ));
            }
        }
        if model.is_public() && acceptance == "organization" {
            return fail(format!(
                "{}: license_acceptance \"organization\" is invalid on a public entry — the \
                 anonymous channel has no principal whose acceptance could be checked",
                model.name
            ));
        }
        if let Some(license) = &model.license {
            let key = (license.as_str(), model.license_version.as_deref());
            let value = (model.license_url.as_deref(), acceptance);
            match terms_documents.get(&key) {
                None => {
                    terms_documents.insert(key, value);
                }
                Some(existing) if *existing == value => {}
                Some(_) => {
                    return fail(format!(
                        "{}: license {license:?} version {:?} appears elsewhere in this index \
                         with a different URL or acceptance policy — one id and version must \
                         identify one document",
                        model.name,
                        model.license_version.as_deref().unwrap_or("-")
                    ));
                }
            }
        }
        for list in [&model.dependencies, &model.postvec_requires] {
            if list.len() > MAX_LIST_ITEMS {
                return fail(format!(
                    "{}: dependency list exceeds {MAX_LIST_ITEMS} items",
                    model.name
                ));
            }
        }
        // Duplicates in the combined requirement list would desynchronize the
        // in-degree bookkeeping below, so they are malformed, not tolerated.
        let mut seen_reqs: BTreeSet<&str> = BTreeSet::new();
        for dep in model.dependencies.iter().chain(&model.postvec_requires) {
            if !seen_reqs.insert(dep.as_str()) {
                return fail(format!(
                    "{}: {dep:?} is listed more than once as a requirement",
                    model.name
                ));
            }
        }
        // Required entitlements: bounded, well-shaped, unique and sorted —
        // and, outside the gateway-base mode, drawn from the enumerated
        // known set. An unknown id is never ignored (§7 guarantee 1).
        if model.required_entitlements.len() > MAX_LIST_ITEMS {
            return fail(format!(
                "{}: required_entitlements exceeds {MAX_LIST_ITEMS} items",
                model.name
            ));
        }
        for (position, id) in model.required_entitlements.iter().enumerate() {
            valid_entitlement_id(id)
                .map_err(|e| SchemaError::Invalid(format!("{}: {e}", model.name)))?;
            if policy == EntitlementIdPolicy::Strict && !KNOWN_ENTITLEMENTS.contains(&id.as_str()) {
                return fail(format!(
                    "{}: unknown entitlement id {id:?} (this build understands {})",
                    model.name,
                    KNOWN_ENTITLEMENTS.join(", ")
                ));
            }
            if position > 0 && model.required_entitlements[position - 1].as_str() >= id.as_str() {
                return fail(format!(
                    "{}: required_entitlements must be unique and sorted",
                    model.name
                ));
            }
        }
        if model.is_public() && !model.required_entitlements.is_empty() {
            return fail(format!(
                "{}: a public entry must not require entitlements — the anonymous channel \
                 has no caller to hold one",
                model.name
            ));
        }
        model
            .archive
            .digest_hex()
            .map_err(|e| SchemaError::Invalid(format!("{}: {e}", model.name)))?;
        if model.archive.size == 0 || model.archive.size > MAX_ARCHIVE_BYTES {
            return fail(format!(
                "{}: implausible archive size {}",
                model.name, model.archive.size
            ));
        }
        if model.archive.installed_size > MAX_ARCHIVE_BYTES {
            return fail(format!(
                "{}: implausible installed size {}",
                model.name, model.archive.installed_size
            ));
        }
        if model.archive.sources.is_empty() || model.archive.sources.len() > MAX_SOURCES {
            return fail(format!(
                "{}: archive must list between 1 and {MAX_SOURCES} sources",
                model.name
            ));
        }
    }

    // Referential integrity + channel containment: every dependency and
    // companion exists, and a public entry never needs a private one. The
    // entitlement-subset rule lives in the same loop so publisher, gateway
    // and client share one implementation: for every requirement edge, the
    // dependency's required-entitlement set must be a subset of the
    // requiring model's — whoever may see a model may also see everything
    // needed to install it, which is what makes filtering a visible model
    // unable to hide something its installation needs.
    for model in &index.models {
        for dep in model.dependencies.iter().chain(&model.postvec_requires) {
            let Some(target) = by_name.get(dep.as_str()) else {
                return fail(format!(
                    "{} requires {dep:?}, which the index does not list",
                    model.name
                ));
            };
            if model.is_public() && !target.is_public() {
                return fail(format!(
                    "public entry {} requires private entry {dep:?}",
                    model.name
                ));
            }
            for required in &target.required_entitlements {
                if !model.required_entitlements.contains(required) {
                    return fail(format!(
                        "{} requires {dep:?}, whose entitlement {required:?} it does not \
                         itself require — a dependency's required_entitlements must be a \
                         subset of the requiring model's",
                        model.name
                    ));
                }
            }
        }
    }

    // The viewer block is response metadata: permitted only on an
    // authenticated document, always refused on a public/anonymous one, and
    // bounded like every other list. (Aphex separately refuses it in any
    // *stored* index — only the gateway knows a document is stored rather
    // than served, so that half of the boundary lives there.)
    if let Some(viewer) = &index.viewer {
        if !index.authenticated {
            return fail(
                "a public/anonymous index must not carry a viewer block — viewer metadata \
                 exists only on authenticated responses"
                    .to_string(),
            );
        }
        if viewer.entitlements.len() > MAX_LIST_ITEMS {
            return fail(format!(
                "viewer.entitlements exceeds {MAX_LIST_ITEMS} items"
            ));
        }
        for (position, entitlement) in viewer.entitlements.iter().enumerate() {
            // Shape only, deliberately not membership in KNOWN_ENTITLEMENTS:
            // a newer gateway may grant an id this build has not learned,
            // and the client only reports it.
            valid_entitlement_id(&entitlement.id)
                .map_err(|e| SchemaError::Invalid(format!("viewer: {e}")))?;
            if position > 0
                && viewer.entitlements[position - 1].id.as_str() >= entitlement.id.as_str()
            {
                return fail("viewer.entitlements must be unique and sorted by id".to_string());
            }
            if !matches!(entitlement.source.as_str(), "default" | "grant") {
                return fail(format!(
                    "viewer: entitlement {:?} has unknown source {:?} (expected \
                     \"default\" or \"grant\")",
                    entitlement.id, entitlement.source
                ));
            }
            if let Some(expires_at) = &entitlement.expires_at {
                if expires_at.is_empty() || expires_at.len() > MAX_EXPIRES_AT_BYTES {
                    return fail(format!(
                        "viewer: entitlement {:?} has an implausible expires_at",
                        entitlement.id
                    ));
                }
            }
        }
    }

    // The combined requirement graph must be acyclic (Kahn's algorithm; both
    // edge kinds count, because `pull` follows both).
    let mut in_degree: BTreeMap<&str, usize> = index
        .models
        .iter()
        .map(|m| {
            (
                m.name.as_str(),
                m.dependencies.len() + m.postvec_requires.len(),
            )
        })
        .collect();
    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut visited = 0usize;
    while let Some(name) = queue.pop() {
        visited += 1;
        for model in &index.models {
            if model
                .dependencies
                .iter()
                .chain(&model.postvec_requires)
                .any(|d| d == name)
            {
                let d = in_degree.get_mut(model.name.as_str()).expect("indexed");
                *d -= 1;
                if *d == 0 {
                    queue.push(model.name.as_str());
                }
            }
        }
    }
    if visited != index.models.len() {
        return fail("registry index contains a dependency cycle".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn entry(name: &str, deps: &[&str], companions: &[&str]) -> IndexModel {
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

    fn private_index(models: Vec<IndexModel>) -> Index {
        Index {
            schema_version: SCHEMA_VERSION,
            channel: "private".to_string(),
            authenticated: true,
            generated_at: None,
            models,
            viewer: None,
        }
    }

    fn private_entry(name: &str, entitlements: &[&str]) -> IndexModel {
        let mut model = entry(name, &[], &[]);
        model.access = "private".to_string();
        model.required_entitlements = entitlements.iter().map(|s| s.to_string()).collect();
        model
    }

    #[test]
    fn a_valid_index_round_trips_through_parse_and_validate() {
        let idx = index(vec![entry("a", &[], &[]), entry("b", &["a"], &[])]);
        let body = serde_json::to_vec(&idx).unwrap();
        let parsed = parse_and_validate(&body).unwrap();
        assert_eq!(parsed.models.len(), 2);
    }

    #[test]
    fn unknown_fields_are_ignored_but_unknown_schema_version_is_fatal() {
        let mut value = serde_json::to_value(index(vec![entry("a", &[], &[])])).unwrap();
        value["future_field"] = serde_json::json!({"x": 1});
        value["models"][0]["another_future_field"] = serde_json::json!(true);
        assert!(parse_and_validate(&serde_json::to_vec(&value).unwrap()).is_ok());

        value["schema_version"] = serde_json::json!(2);
        let err = parse_and_validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(err, SchemaError::SchemaVersion(2)), "{err}");
    }

    /// `access` is required — the gateway serves a mixed catalogue and
    /// defaulting a missing access class would be the unsafe direction.
    #[test]
    fn a_missing_access_class_is_a_parse_failure() {
        let mut value = serde_json::to_value(index(vec![entry("a", &[], &[])])).unwrap();
        value["models"][0].as_object_mut().unwrap().remove("access");
        let err = parse_and_validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(err, SchemaError::Json(_)), "{err}");

        let mut bad = entry("a", &[], &[]);
        bad.access = "internal".to_string();
        let err = validate(&index(vec![bad])).unwrap_err();
        assert!(err.to_string().contains("internal"), "{err}");
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let idx = index(vec![entry("a", &[], &[]), entry("a", &[], &[])]);
        assert!(validate(&idx)
            .unwrap_err()
            .to_string()
            .contains("more than once"));
    }

    #[test]
    fn a_missing_dependency_is_rejected() {
        let idx = index(vec![entry("b", &["ghost"], &[])]);
        assert!(validate(&idx).unwrap_err().to_string().contains("ghost"));
    }

    #[test]
    fn a_public_entry_may_not_require_a_private_one() {
        let mut private = entry("p", &[], &[]);
        private.access = "private".to_string();
        let idx = private_index(vec![private, entry("pub", &["p"], &[])]);
        let err = validate(&idx).unwrap_err().to_string();
        assert!(err.contains("public entry"), "{err}");
    }

    /// Channel ↔ authentication ↔ access containment: an anonymously served
    /// public document may not list a private entry, and the authenticated
    /// flag must match the channel in both directions.
    #[test]
    fn channel_containment_binds_access_and_authentication() {
        let mut hidden = entry("p", &[], &[]);
        hidden.access = "private".to_string();
        let err = validate(&index(vec![hidden])).unwrap_err().to_string();
        assert!(err.contains("may not list a private entry"), "{err}");

        let mut lying_public = index(vec![entry("a", &[], &[])]);
        lying_public.authenticated = true;
        let err = validate(&lying_public).unwrap_err().to_string();
        assert!(err.contains("must not claim authenticated"), "{err}");

        let mut lying_private = private_index(vec![entry("a", &[], &[])]);
        lying_private.authenticated = false;
        let err = validate(&lying_private).unwrap_err().to_string();
        assert!(err.contains("must set authenticated"), "{err}");
    }

    #[test]
    fn a_dependency_cycle_is_rejected() {
        let idx = index(vec![entry("a", &["b"], &[]), entry("b", &["a"], &[])]);
        assert!(validate(&idx).unwrap_err().to_string().contains("cycle"));
    }

    #[test]
    fn bad_digests_and_sizes_are_rejected() {
        let mut bad = entry("a", &[], &[]);
        bad.archive.digest = "sha256:short".to_string();
        assert!(validate(&index(vec![bad])).is_err());

        let mut bad = entry("a", &[], &[]);
        bad.archive.digest = format!("md5:{}", "ab".repeat(32));
        assert!(validate(&index(vec![bad])).is_err());

        let mut bad = entry("a", &[], &[]);
        bad.archive.size = 0;
        assert!(validate(&index(vec![bad])).is_err());

        let mut bad = entry("a", &[], &[]);
        bad.archive.sources.clear();
        assert!(validate(&index(vec![bad])).is_err());
    }

    #[test]
    fn zero_dimensions_and_bad_versions_are_rejected() {
        let mut bad = entry("a", &[], &[]);
        bad.target_dim = Some(0);
        assert!(validate(&index(vec![bad]))
            .unwrap_err()
            .to_string()
            .contains("target_dim"));

        let mut bad = entry("a", &[], &[]);
        bad.min_postvec_version = Some("unreleased".into());
        assert!(validate(&index(vec![bad]))
            .unwrap_err()
            .to_string()
            .contains("min_postvec_version"));
    }

    #[test]
    fn duplicate_requirements_are_rejected() {
        let dep = entry("dep", &[], &[]);
        let mut bad = entry("a", &["dep"], &["dep"]);
        bad.model_type = "convert".into();
        assert!(validate(&index(vec![dep, bad]))
            .unwrap_err()
            .to_string()
            .contains("more than once as a requirement"));
    }

    #[test]
    fn hostile_model_names_are_rejected() {
        for name in [
            "", "../x", "a/b", "a\\b", ".hidden", "-flag", "UPPER", "a b", "a\0b",
        ] {
            assert!(
                valid_model_name(name).is_err(),
                "{name:?} should be invalid"
            );
        }
        for name in [
            "a",
            "baai-bge-m3",
            "convert-bge_base-to-openai_ada_002",
            "e5.v2",
        ] {
            assert!(valid_model_name(name).is_ok(), "{name:?} should be valid");
        }
    }

    #[test]
    fn sha256_digest_parsing_accepts_only_64_lowercase_hex() {
        let good = format!("sha256:{}", "a".repeat(64));
        assert_eq!(parse_sha256_digest(&good), Some("a".repeat(64).as_str()));
        assert!(parse_sha256_digest(&"a".repeat(64)).is_none());
        assert!(parse_sha256_digest("sha256:abc").is_none());
        assert!(parse_sha256_digest(&format!("sha256:{}", "A".repeat(64))).is_none());
        assert!(parse_sha256_digest(&format!("sha256:{}", "g".repeat(64))).is_none());
        assert!(parse_sha256_digest("sha256:../../../etc/passwd").is_none());
    }

    /// `revision` is additive at schema_version 1: an absent value reads as
    /// 1 (so nothing published before updatable names needs republishing),
    /// and out-of-range values are rejected rather than silently ignored.
    #[test]
    fn the_revision_is_optional_but_validated() {
        let plain = entry("a", &[], &[]);
        assert_eq!(plain.revision(), 1);

        let mut updated = entry("a", &[], &[]);
        updated.revision = Some(4);
        assert_eq!(updated.revision(), 4);
        let body = serde_json::to_vec(&index(vec![updated])).unwrap();
        assert_eq!(parse_and_validate(&body).unwrap().models[0].revision(), 4);

        for bad in [0, MAX_REVISION + 1] {
            let mut model = entry("a", &[], &[]);
            model.revision = Some(bad);
            let err = validate(&index(vec![model])).unwrap_err().to_string();
            assert!(err.contains("revision"), "{err}");
        }
    }

    #[test]
    fn version_parsing_tolerates_package_suffixes() {
        for ok in ["0.8.0", "0.8", "0.8.5-1.pgdg22.04+1", "1.0.0"] {
            assert!(version_parses(ok), "{ok}");
        }
        for bad in ["", "unreleased", "not-a-version"] {
            assert!(!version_parses(bad), "{bad}");
        }
    }

    /// M3: `required_entitlements` is additive at schema v1 — the version
    /// does not move, an absent list reads as empty, and a current client
    /// parses an entry carrying requirements without error.
    #[test]
    fn required_entitlements_are_additive_at_schema_v1() {
        assert_eq!(SCHEMA_VERSION, 1);

        let plain = entry("a", &[], &[]);
        assert!(plain.required_entitlements.is_empty());

        let idx = private_index(vec![private_entry("paid", &["postvec-catalog"])]);
        let body = serde_json::to_vec(&idx).unwrap();
        let parsed = parse_and_validate(&body).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.models[0].required_entitlements, ["postvec-catalog"]);

        // An empty list is omitted from the serialized document.
        let body = serde_json::to_vec(&private_index(vec![private_entry("free", &[])])).unwrap();
        assert!(!String::from_utf8(body)
            .unwrap()
            .contains("required_entitlements"));
    }

    /// Unknown, duplicate, unsorted, malformed and oversized requirement
    /// lists all fail strict validation; the gateway-base mode relaxes
    /// exactly one of those rules — the unknown id — and nothing else.
    #[test]
    fn requirement_lists_are_validated_hard() {
        // Unknown id: refused strictly, admitted by the gateway-base mode
        // (which leaves it for the filter to treat as unheld).
        let idx = private_index(vec![private_entry("m", &["future-tier"])]);
        let err = validate(&idx).unwrap_err().to_string();
        assert!(err.contains("unknown entitlement id"), "{err}");
        validate_gateway_base(&idx).unwrap();
        let body = serde_json::to_vec(&idx).unwrap();
        assert!(parse_and_validate(&body).is_err());
        parse_and_validate_gateway_base(&body).unwrap();

        // Duplicates and unsorted lists fail in BOTH modes. (Two known ids
        // do not exist yet, so the unsorted case rides gateway-base ids.)
        let dup = private_index(vec![private_entry(
            "m",
            &["postvec-catalog", "postvec-catalog"],
        )]);
        for result in [validate(&dup), validate_gateway_base(&dup)] {
            let err = result.unwrap_err().to_string();
            assert!(err.contains("unique and sorted"), "{err}");
        }
        let unsorted = private_index(vec![private_entry("m", &["b-tier", "a-tier"])]);
        let err = validate_gateway_base(&unsorted).unwrap_err().to_string();
        assert!(err.contains("unique and sorted"), "{err}");

        // Malformed ids fail shape validation in both modes.
        for bad in ["", "UPPER", "-leading", "a b", "under_score"] {
            let idx = private_index(vec![private_entry("m", &[bad])]);
            assert!(validate(&idx).is_err(), "{bad:?}");
            assert!(validate_gateway_base(&idx).is_err(), "{bad:?}");
        }
        let oversized = private_index(vec![private_entry("m", &["x".repeat(65).as_str()])]);
        assert!(validate_gateway_base(&oversized).is_err());

        // An oversized list fails in both modes.
        let many: Vec<String> = (0..=MAX_LIST_ITEMS)
            .map(|i| format!("tier-{i:03}"))
            .collect();
        let mut model = private_entry("m", &[]);
        model.required_entitlements = many;
        let idx = private_index(vec![model]);
        let err = validate_gateway_base(&idx).unwrap_err().to_string();
        assert!(err.contains("required_entitlements exceeds"), "{err}");
    }

    /// A public entry must not require entitlements: the anonymous channel
    /// has no caller to hold one.
    #[test]
    fn a_public_entry_with_a_requirement_is_refused() {
        let mut model = entry("m", &[], &[]);
        model.required_entitlements = vec!["postvec-catalog".to_string()];
        // On the private (mixed) channel, so the public-entry rule fires
        // rather than the channel-containment rule.
        let err = validate(&private_index(vec![model]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not require entitlements"), "{err}");
    }

    /// The subset invariant holds across both edge kinds: a dependency's
    /// required-entitlement set must be a subset of its parent's, so
    /// filtering a visible model can never hide something needed to
    /// install it. An unrestricted parent with a restricted dependency
    /// fails, in strict AND gateway-base mode.
    #[test]
    fn the_dependency_subset_rule_holds_across_both_edge_kinds() {
        for edge in ["dependencies", "postvec_requires"] {
            let restricted = private_entry("restricted-dep", &["postvec-catalog"]);
            let mut parent = private_entry("parent", &[]);
            match edge {
                "dependencies" => parent.dependencies = vec!["restricted-dep".to_string()],
                _ => parent.postvec_requires = vec!["restricted-dep".to_string()],
            }
            let idx = private_index(vec![restricted.clone(), parent.clone()]);
            for result in [validate(&idx), validate_gateway_base(&idx)] {
                let err = result.unwrap_err().to_string();
                assert!(err.contains("subset"), "{edge}: {err}");
            }

            // Equal (or wider) parent requirements satisfy the rule.
            let mut entitled_parent = parent.clone();
            entitled_parent.required_entitlements = vec!["postvec-catalog".to_string()];
            validate(&private_index(vec![restricted, entitled_parent])).unwrap();
        }
    }

    /// The viewer block: permitted only on authenticated documents, bounded,
    /// deterministically ordered, with an enumerated source — and always
    /// refused on a public/anonymous document.
    #[test]
    fn the_viewer_block_is_authenticated_only_and_bounded() {
        let viewer = |entitlements: Vec<ViewerEntitlement>| Viewer { entitlements };
        let ent = |id: &str, source: &str| ViewerEntitlement {
            id: id.to_string(),
            source: source.to_string(),
            expires_at: None,
        };

        // Valid on an authenticated document, round-tripping through serde.
        let mut idx = private_index(vec![entry("a", &[], &[])]);
        idx.viewer = Some(viewer(vec![ent("postvec-catalog", "default")]));
        let body = serde_json::to_vec(&idx).unwrap();
        let parsed = parse_and_validate(&body).unwrap();
        let parsed_viewer = parsed.viewer.as_ref().unwrap();
        assert_eq!(parsed_viewer.entitlements[0].id, "postvec-catalog");
        assert_eq!(parsed_viewer.entitlements[0].source, "default");
        assert!(parsed_viewer.entitlements[0].expires_at.is_none());

        // Refused outright on a public/anonymous document.
        let mut public = index(vec![entry("a", &[], &[])]);
        public.viewer = Some(viewer(vec![ent("postvec-catalog", "default")]));
        let err = validate(&public).unwrap_err().to_string();
        assert!(err.contains("must not carry a viewer"), "{err}");

        // Unknown source, unsorted/duplicate ids, malformed id, oversized
        // list and implausible expiry all fail.
        let cases: Vec<(Viewer, &str)> = vec![
            (viewer(vec![ent("postvec-catalog", "billing")]), "source"),
            (
                viewer(vec![ent("b-tier", "default"), ent("a-tier", "default")]),
                "sorted",
            ),
            (
                viewer(vec![
                    ent("postvec-catalog", "default"),
                    ent("postvec-catalog", "grant"),
                ]),
                "sorted",
            ),
            (viewer(vec![ent("UPPER", "default")]), "entitlement id"),
            (
                viewer(
                    (0..=MAX_LIST_ITEMS)
                        .map(|i| ent(&format!("t-{i:03}"), "grant"))
                        .collect(),
                ),
                "exceeds",
            ),
            (
                viewer(vec![ViewerEntitlement {
                    id: "postvec-catalog".to_string(),
                    source: "grant".to_string(),
                    expires_at: Some("x".repeat(65)),
                }]),
                "expires_at",
            ),
        ];
        for (bad_viewer, expected) in cases {
            let mut idx = private_index(vec![entry("a", &[], &[])]);
            idx.viewer = Some(bad_viewer);
            let err = validate(&idx).unwrap_err().to_string();
            assert!(err.contains(expected), "{expected}: {err}");
        }

        // A bounded expiry from a grant is fine.
        let mut idx = private_index(vec![entry("a", &[], &[])]);
        idx.viewer = Some(viewer(vec![ViewerEntitlement {
            id: "postvec-catalog".to_string(),
            source: "grant".to_string(),
            expires_at: Some("2027-01-01T00:00:00Z".to_string()),
        }]));
        validate(&idx).unwrap();
    }

    /// The gateway-base mode relaxes ONLY the known-id rule: every other
    /// structural rule still fires.
    #[test]
    fn the_gateway_base_mode_retains_every_structural_rule() {
        let mut bad_digest = private_entry("m", &["future-tier"]);
        bad_digest.archive.digest = "sha256:nope".to_string();
        assert!(validate_gateway_base(&private_index(vec![bad_digest])).is_err());

        let ghost = {
            let mut m = private_entry("m", &["future-tier"]);
            m.dependencies = vec!["ghost".to_string()];
            m
        };
        assert!(validate_gateway_base(&private_index(vec![ghost])).is_err());

        let mut public_restricted = entry("m", &[], &[]);
        public_restricted.required_entitlements = vec!["future-tier".to_string()];
        let err = validate_gateway_base(&private_index(vec![public_restricted]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not require entitlements"), "{err}");
    }

    /// M2 compatibility: the terms fields are additive at schema v1 — an
    /// index with none of them parses unchanged, absent acceptance reads as
    /// `none`, and absent fields are omitted from the serialized document
    /// (so publishing the same catalogue byte-reproduces the same index).
    #[test]
    fn terms_fields_are_additive_at_schema_v1() {
        assert_eq!(SCHEMA_VERSION, 1);

        let plain = entry("a", &[], &[]);
        assert_eq!(plain.license_acceptance(), "none");
        let body = serde_json::to_vec(&index(vec![plain])).unwrap();
        let text = String::from_utf8(body.clone()).unwrap();
        for absent in ["license_version", "license_url", "license_acceptance"] {
            assert!(!text.contains(absent), "{absent} must be omitted");
        }
        parse_and_validate(&body).unwrap();

        // A fully versioned notice document round-trips.
        let mut notice = entry("a", &[], &[]);
        notice.license_version = Some("2026-08-09".to_string());
        notice.license_url = Some("https://univec.ai/legal/models/x/2026-08-09".to_string());
        notice.license_acceptance = Some("notice".to_string());
        let body = serde_json::to_vec(&index(vec![notice])).unwrap();
        let parsed = parse_and_validate(&body).unwrap();
        assert_eq!(parsed.models[0].license_acceptance(), "notice");
        assert_eq!(
            parsed.models[0].license_version.as_deref(),
            Some("2026-08-09")
        );
    }

    /// The M2 validation rules: half-present document identity, invalid
    /// versions, non-HTTPS or credentialed URLs, unknown acceptance values,
    /// notice/organization without a document, terms without a licence id,
    /// and organization on a public entry all fail — in gateway-base mode
    /// too, which relaxes only the known-entitlement rule.
    #[test]
    fn terms_metadata_is_validated_hard() {
        let versioned = |version: Option<&str>, url: Option<&str>, acceptance: Option<&str>| {
            let mut model = entry("m", &[], &[]);
            model.license_version = version.map(str::to_string);
            model.license_url = url.map(str::to_string);
            model.license_acceptance = acceptance.map(str::to_string);
            model
        };
        let url = "https://univec.ai/legal/x/1";
        let cases: Vec<(IndexModel, &str)> = vec![
            (
                versioned(Some("1"), None, None),
                "both present or both absent",
            ),
            (
                versioned(None, Some(url), None),
                "both present or both absent",
            ),
            (
                versioned(None, None, Some("notice")),
                "requires a license_version",
            ),
            (
                versioned(None, None, Some("organization")),
                "requires a license_version",
            ),
            (
                versioned(Some("1"), Some(url), Some("click-through")),
                "unknown license_acceptance",
            ),
            (
                versioned(Some(""), Some(url), None),
                "license_version is empty",
            ),
            (versioned(Some("1 2"), Some(url), None), "forbidden byte"),
            (versioned(Some("1@2"), Some(url), None), "forbidden byte"),
            (
                versioned(Some(&"v".repeat(65)), Some(url), None),
                "license_version exceeds",
            ),
            (
                versioned(Some("1"), Some("http://univec.ai/x"), None),
                "not an absolute https",
            ),
            (
                versioned(Some("1"), Some("https:///no-host"), None),
                "has no host",
            ),
            (
                versioned(Some("1"), Some("https://user:pw@univec.ai/x"), None),
                "embeds credentials",
            ),
            (
                versioned(
                    Some("1"),
                    Some(&format!("https://univec.ai/{}", "u".repeat(512))),
                    None,
                ),
                "license_url exceeds",
            ),
            (
                versioned(Some("1"), Some(url), Some("organization")),
                "invalid on a public entry",
            ),
            (
                {
                    let mut model = versioned(Some("1"), Some(url), None);
                    model.license = None;
                    model
                },
                "without a license id",
            ),
        ];
        for (model, expected) in cases {
            let idx = index(vec![model]);
            for result in [validate(&idx), validate_gateway_base(&idx)] {
                let err = result.unwrap_err().to_string();
                assert!(err.contains(expected), "{expected}: {err}");
            }
        }

        // Organization IS representable on a private entry (M4b's wire
        // format), and notice with a full document is valid on both.
        let mut org = versioned(Some("1"), Some(url), Some("organization"));
        org.access = "private".to_string();
        validate(&private_index(vec![org])).unwrap();
        let notice = versioned(Some("1"), Some(url), Some("notice"));
        validate(&index(vec![notice])).unwrap();
    }

    /// Within one index, the same licence id and version cannot point at
    /// different URLs or acceptance policies — one id@version is one
    /// document. An explicit `"none"` equals an absent acceptance.
    #[test]
    fn one_license_id_and_version_identifies_one_document() {
        let doc = |name: &str, url: &str, acceptance: Option<&str>| {
            let mut model = entry(name, &[], &[]);
            model.license_version = Some("1".to_string());
            model.license_url = Some(url.to_string());
            model.license_acceptance = acceptance.map(str::to_string);
            model
        };
        let url = "https://univec.ai/legal/x/1";

        // Same document twice: fine.
        let idx = index(vec![
            doc("a", url, Some("notice")),
            doc("b", url, Some("notice")),
        ]);
        validate(&idx).unwrap();

        // Same id+version, different URL: refused.
        let idx = index(vec![
            doc("a", url, None),
            doc("b", "https://univec.ai/legal/other/1", None),
        ]);
        let err = validate(&idx).unwrap_err().to_string();
        assert!(
            err.contains("one id and version must identify one document"),
            "{err}"
        );

        // Same id+version, different acceptance policy: refused.
        let idx = index(vec![doc("a", url, Some("notice")), doc("b", url, None)]);
        let err = validate(&idx).unwrap_err().to_string();
        assert!(err.contains("different URL or acceptance"), "{err}");

        // Explicit "none" and absent acceptance are the same policy, and
        // unversioned entries under one id agree with each other.
        let mut explicit = entry("a", &[], &[]);
        explicit.license_acceptance = Some("none".to_string());
        validate(&index(vec![explicit, entry("b", &[], &[])])).unwrap();
    }

    #[test]
    fn license_version_and_url_shapes_are_validated() {
        for ok in ["1", "2026-08-09", "1.2.3-rc1", "v2"] {
            assert!(valid_license_version(ok).is_ok(), "{ok}");
        }
        for bad in ["", "1 2", "1@2", "1\t2", "1\n", "ver\u{7f}sion", "é"] {
            assert!(valid_license_version(bad).is_err(), "{bad:?}");
        }
        for ok in [
            "https://univec.ai/legal/models/univec-commercial/2026-08-09",
            "https://example.com",
            "https://example.com:8443/x",
        ] {
            assert!(valid_license_url(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            "http://univec.ai/x",
            "https://",
            "https:///path",
            "https://:443/path",
            "https://user@host/x",
            "ftp://univec.ai/x",
            "univec.ai/x",
            "https://univec.ai/with space",
        ] {
            assert!(valid_license_url(bad).is_err(), "{bad:?}");
        }
    }

    /// A licence id shares the model-name grammar; anything else is refused
    /// at the schema, so a schema-valid entry is always expressible as an
    /// `--accept-license <id>@<version>` token — the CLI enforces the same
    /// rule through these very validators.
    #[test]
    fn license_ids_share_the_token_grammar() {
        for ok in ["mit", "apache-2.0", "bsd-3-clause", "univec-commercial"] {
            assert!(valid_license_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "MIT", "-x", "a b", "a@b", "terms/v1", "é"] {
            assert!(valid_license_id(bad).is_err(), "{bad:?}");
        }

        // Enforced by index validation, in strict AND gateway-base mode —
        // even on an entry with no other terms metadata.
        let mut model = entry("m", &[], &[]);
        model.license = Some("Weird Terms".to_string());
        let idx = index(vec![model]);
        for result in [validate(&idx), validate_gateway_base(&idx)] {
            let err = result.unwrap_err().to_string();
            assert!(err.contains("license id"), "{err}");
        }

        // The acknowledgeability guarantee: for any valid notice entry, both
        // halves of the token pass the validators the CLI parses flags with.
        let mut notice = entry("m", &[], &[]);
        notice.license = Some("special-terms".to_string());
        notice.license_version = Some("2026-08-09".to_string());
        notice.license_url = Some("https://univec.ai/legal/x/2026-08-09".to_string());
        notice.license_acceptance = Some("notice".to_string());
        validate(&index(vec![notice.clone()])).unwrap();
        valid_license_id(notice.license.as_deref().unwrap()).unwrap();
        valid_license_version(notice.license_version.as_deref().unwrap()).unwrap();
    }

    #[test]
    fn entitlement_id_shape_is_validated() {
        for ok in ["postvec-catalog", "a", "tier-2"] {
            assert!(valid_entitlement_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-x", "X", "a_b", "a.b", "a b"] {
            assert!(valid_entitlement_id(bad).is_err(), "{bad:?}");
        }
    }
}
