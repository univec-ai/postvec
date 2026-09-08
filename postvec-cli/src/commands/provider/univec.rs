//! What `provider add univec` knows that no other connector needs: the
//! public catalogue (`GET /v1/models`), the selectors over it, the offer to
//! reuse the `postvec login` key, and the best-effort unbilled identity
//! check that runs before any billed probe.

use super::add::{ConvertSpec, KeySpec, NewModel};
use crate::cli::ProviderAddArgs;
use crate::error::{CliError, Result};
use crate::output::Output;
use providers::listing::{self, ListedKind, ListedModel};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The converter selectors, validated as a unit.
pub struct Selectors {
    pub convert: Vec<(String, String)>,
    pub convert_to: Vec<String>,
    pub convert_from: Vec<String>,
    pub all_converters: bool,
}

impl Selectors {
    /// `legacy` is the manual five-flag converter; `embeds` the `--model`
    /// count. Every combination the flags allow but the command does not
    /// is refused here with the reason.
    pub fn from_args(
        args: &ProviderAddArgs,
        canonical: &str,
        legacy: bool,
        embeds: usize,
    ) -> Result<Self> {
        let usage = |m: &str| Err(CliError::usage(m.to_string()));
        let mut convert = Vec::new();
        for pair in &args.convert {
            match pair.split_once(':') {
                Some((src, dst)) if !src.trim().is_empty() && !dst.trim().is_empty() => {
                    convert.push((src.trim().to_string(), dst.trim().to_string()))
                }
                _ => {
                    return usage(&format!(
                        "--convert {pair:?}: expected SRC:DST, two UniVec model names"
                    ))
                }
            }
        }
        let selectors = Self {
            convert,
            convert_to: args.convert_to.clone(),
            convert_from: args.convert_from.clone(),
            all_converters: args.all_converters,
        };
        let univec_only = selectors.any() || args.api_key_from_login || args.no_catalog;
        if univec_only && canonical != "univec" {
            return usage("--convert, --convert-to, --convert-from, --all-converters, --api-key-from-login and --no-catalog apply to provider \"univec\" only");
        }
        if canonical != "univec" && embeds == 0 && !legacy {
            return usage(
                "--model is required (only univec can add its whole catalogue without one)",
            );
        }
        if legacy && (selectors.any() || embeds > 0) {
            return usage("the manual converter flags (--convert-source …) add exactly one entry and cannot be combined with --model or the catalogue selectors");
        }
        if args.no_catalog && selectors.bulk() {
            return usage("--convert-to, --convert-from and --all-converters need the catalogue; drop --no-catalog");
        }
        if args.no_catalog && embeds == 0 && !legacy && selectors.convert.is_empty() {
            return usage("--no-catalog needs --model (or the manual converter flags): there is nothing to add");
        }
        if args.no_catalog && !selectors.convert.is_empty() {
            return usage("--convert needs the catalogue for its dimensions; use the manual converter flags with --no-catalog");
        }
        if args.converter_name.is_some()
            && !legacy
            && (selectors.convert.len() != 1 || selectors.bulk())
        {
            return usage("--converter-name applies to a single --convert SRC:DST (or the manual converter flags)");
        }
        Ok(selectors)
    }

    pub fn any(&self) -> bool {
        !self.convert.is_empty() || self.bulk()
    }

    pub fn bulk(&self) -> bool {
        !self.convert_to.is_empty() || !self.convert_from.is_empty() || self.all_converters
    }

    /// The catalogue is the only source for these; without it the run
    /// cannot proceed. `--model` and `--convert` can fall back or fail
    /// per entry.
    pub fn needs_catalogue(&self, embeds: usize, legacy: bool) -> bool {
        self.bulk() || (embeds == 0 && !legacy && self.convert.is_empty())
    }

    /// The converters this selection names, in catalogue order.
    pub fn converters<'a>(&self, catalogue: &'a [ListedModel]) -> Result<Vec<&'a ListedModel>> {
        let all = || catalogue.iter().filter(|m| m.kind == ListedKind::Convert);
        let mut picked: Vec<&ListedModel> = Vec::new();
        let mut push = |m: &'a ListedModel| {
            if !picked.iter().any(|p| std::ptr::eq(*p, m)) {
                picked.push(m);
            }
        };
        for (src, dst) in &self.convert {
            match listing::converter(catalogue, src, dst) {
                Some(m) => push(m),
                None => {
                    return Err(CliError::precondition(format!(
                        "UniVec's catalogue has no converter {src} -> {dst}"
                    ))
                    .with_fix(
                        "check `postvec provider ls --available univec`; for a pair UniVec has \
                         not listed yet, use the manual flags --convert-source/--convert-target/\
                         --source-model/--target-model/--source-dim",
                    ))
                }
            }
        }
        let narrow = |flag: &str,
                      wanted: &str,
                      pick: &dyn Fn(&ListedModel) -> &str|
         -> Result<Vec<&'a ListedModel>> {
            let hits: Vec<&ListedModel> = all().filter(|m| pick(m) == wanted).collect();
            if hits.is_empty() {
                let mut names: Vec<&str> = all().map(pick).collect();
                names.sort_unstable();
                names.dedup();
                return Err(CliError::precondition(format!(
                    "{flag} {wanted}: no converter in UniVec's catalogue; the catalogue's {} are: {}",
                    if flag == "--convert-to" { "targets" } else { "sources" },
                    names.join(", ")
                )));
            }
            Ok(hits)
        };
        for target in &self.convert_to {
            for m in narrow("--convert-to", target, &|m| m.provider_model_id.as_str())? {
                push(m);
            }
        }
        for source in &self.convert_from {
            for m in narrow("--convert-from", source, &|m| {
                m.source.as_ref().map(|(s, _)| s.as_str()).unwrap_or("")
            })? {
                push(m);
            }
        }
        if self.all_converters {
            all().for_each(push);
        }
        Ok(picked)
    }
}

/// Fetch the catalogue. `required`: a failure is the command's failure;
/// otherwise a warning and `None`, and the run continues on the probe.
pub async fn fetch_catalogue(
    base_url: Option<&str>,
    timeout: Duration,
    required: bool,
    output: &Output,
) -> Result<Option<Vec<ListedModel>>> {
    match listing::list_models("univec", base_url, timeout, true).await {
        Ok(listing::Listing::Entries(models)) => Ok(Some(models)),
        Ok(other) => Err(CliError::internal(format!(
            "univec listing answered {other:?}"
        ))),
        // A catalogue that answered but breaks its contract is not an
        // outage to work around: nothing is written or billed.
        Err(e @ providers::EmbeddingError::Api { status: 200, .. }) => Err(CliError::precondition(
            e.to_string(),
        )
        .with_fix("report the row to UniVec; --no-catalog with explicit --dim bypasses discovery")),
        Err(e) if required => Err(CliError::precondition(format!(
            "cannot fetch UniVec's catalogue from {}/v1/models: {e}",
            base_url.unwrap_or(providers::univec::DEFAULT_UNIVEC_BASE_URL)
        ))
        .with_fix(
            "retry later, or name models explicitly: --model ID (the probe measures the \
             dimension) or the manual converter flags",
        )),
        Err(e) => {
            output.note(&format!(
                "UniVec's catalogue is unreachable ({e}); continuing without it — the probe \
                 measures dimensions"
            ));
            Ok(None)
        }
    }
}

/// A catalogue converter as the entry `provider add` writes. Both
/// vocabularies carry UniVec's public names: the registry's names are
/// aphex's names by construction.
pub(super) fn converter_model(
    m: &ListedModel,
    catalogue: &[ListedModel],
    name: Option<&str>,
) -> Result<NewModel> {
    listing::check_converter_dims(catalogue, m).map_err(CliError::precondition)?;
    let (source, source_dim) = m.source.clone().expect("a converter has a source");
    let target = m.provider_model_id.clone();
    let public_name = name
        .map(str::to_string)
        .unwrap_or_else(|| format!("univec-convert-{source}-to-{target}"));
    providers::catalog::validate_public_name(&public_name)
        .map_err(|e| CliError::usage(format!("--converter-name: {e}")))?;
    Ok(NewModel {
        placeholder_dim: m.dim,
        id: target.clone(),
        public_name,
        dim: Some(m.dim),
        max_tokens: None,
        max_batch: None,
        catalogued: true,
        convert: Some(ConvertSpec {
            provider_source_id: source.clone(),
            source_model: source,
            target_model: target,
            source_dim,
        }),
        space: None,
        prefer: false,
    })
}

// ---- The login-key offer --------------------------------------------------

/// Every stored UniVec credential this run could copy, as `(key, source)`:
/// `POSTVEC_API_KEY`, the effective user's store, and — under `sudo` —
/// the invoking user's own store, read as root from that user's passwd
/// home (`sudo` resets `$HOME`). A malformed store is a note, not a stop.
fn stored_credentials(output: &Output) -> Vec<(String, String)> {
    use crate::registry::auth;
    let mut found = Vec::new();
    match auth::resolve(None) {
        Ok(Some(c)) => found.push((c.key, c.source.to_string())),
        Ok(None) => {}
        Err(e) => output.note(&format!("stored credential not usable: {e}")),
    }
    let sudo_user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| crate::proc::is_root() && !u.is_empty() && u != "root");
    if let Some(user) = sudo_user {
        let store = crate::proc::home_dir(&user).map(|home| home.join(".config/postvec/auth.json"));
        if let (Ok(account), Ok(store)) = (crate::proc::OsAccount::lookup(&user), store) {
            match auth::read_private_store(&store, account.uid) {
                Ok(Some(key)) => found.push((key, format!("{user}'s postvec login"))),
                Ok(None) => {}
                Err(e) => output.note(&format!("{}: {e}", store.display())),
            }
        }
    }
    found
}

/// Which credential to offer. `POSTVEC_API_KEY` wins outright (it is an
/// explicit choice for this run); otherwise root's store and the invoking
/// user's store are peers, and two different keys are an ambiguity the
/// operator resolves — interactively by picking, in a script by naming a
/// source — never by precedence.
enum Choice {
    None,
    One(String, String),
    Ambiguous(Vec<(String, String)>),
}

fn choose(found: Vec<(String, String)>) -> Choice {
    let env = crate::registry::auth::API_KEY_ENV;
    if let Some(hit) = found.iter().find(|(_, source)| source == env) {
        return Choice::One(hit.0.clone(), hit.1.clone());
    }
    let mut distinct: Vec<(String, String)> = Vec::new();
    for (key, source) in found {
        match distinct.iter_mut().find(|(k, _)| *k == key) {
            Some((_, sources)) => {
                sources.push_str(" and ");
                sources.push_str(&source);
            }
            None => distinct.push((key, source)),
        }
    }
    match distinct.len() {
        0 => Choice::None,
        1 => {
            let (key, source) = distinct.remove(0);
            Choice::One(key, source)
        }
        _ => Choice::Ambiguous(distinct),
    }
}

/// The offer, or the scripted opt-in. `None` falls through to the ordinary
/// key sources. Never implicit: an interactive run asks (default no), a
/// non-interactive one needs `--api-key-from-login`.
pub(super) fn login_key_offer(
    args: &ProviderAddArgs,
    stem: &str,
    providers_dir: &Path,
    existing: Option<&super::ProviderFileDoc>,
    owner: Option<super::FileOwner>,
    output: &Output,
) -> Result<Option<KeySpec>> {
    let path = key_path(providers_dir, stem);
    let interactive = crate::proc::is_stdin_tty() && !output.is_json();
    if !args.api_key_from_login && !interactive {
        return Ok(None);
    }
    let masked = crate::registry::auth::masked_key;
    let (key, source) =
        match choose(stored_credentials(output)) {
            Choice::None if args.api_key_from_login => return Err(CliError::usage(
                "--api-key-from-login: no stored credential (POSTVEC_API_KEY, or `postvec login`)",
            )),
            Choice::None => return Ok(None),
            Choice::One(key, source) => (key, source),
            Choice::Ambiguous(choices) if !interactive => {
                let listed: Vec<String> = choices
                    .iter()
                    .map(|(key, source)| format!("{} from {source}", masked(key)))
                    .collect();
                return Err(CliError::usage(format!(
                    "--api-key-from-login: two different stored keys ({}); set POSTVEC_API_KEY to \
                 the one to use, or `postvec logout` the other",
                    listed.join(", ")
                )));
            }
            Choice::Ambiguous(choices) => {
                eprintln!("Two different UniVec keys are stored:");
                for (i, (key, source)) in choices.iter().enumerate() {
                    eprintln!("  {}. {} from {source}", i + 1, masked(key));
                }
                eprint!("Use which for hosted inference? [1-{}/N] ", choices.len());
                let answer = crate::plan::read_line()?;
                match answer.trim().parse::<usize>() {
                    Ok(n) if (1..=choices.len()).contains(&n) => choices[n - 1].clone(),
                    _ => return Ok(None),
                }
            }
        };
    let destination = preflight_key_destination(
        &path,
        &key,
        args.replace_copied_key,
        providers_dir,
        stem,
        owner,
    )?;
    // Already the connector's key source, byte for byte: a real no-op, not
    // a credential change that re-verifies the file.
    if matches!(destination, Destination::Identical(_))
        && existing.is_some_and(|doc| {
            doc.value.get("api_key_file").and_then(toml::Value::as_str)
                == Some(path.display().to_string().as_str())
        })
    {
        output.note(&format!(
            "the connector already references the stored key at {}; nothing to change",
            path.display()
        ));
        return Ok(Some(KeySpec::Existing));
    }
    let verb = match &destination {
        Destination::Absent => "copied to",
        Destination::Identical(_) => "already at",
        Destination::Rotate { .. } => "rotated in",
    };
    if args.api_key_from_login {
        output.note(&format!(
            "using the key from {source}; {verb} {}",
            path.display()
        ));
        return Ok(Some(KeySpec::Login {
            key,
            path,
            destination,
        }));
    }
    eprintln!(
        "Found a UniVec key from {source} ({}). Use it for hosted inference too?\n\
         It will be billed from this host. A key created with a $0 spending limit (the login \
         prompt's advice for downloads) fails with 402; a dedicated inference key from {} \
         keeps billing and rotation separate.",
        masked(&key),
        crate::registry::urls::DASHBOARD_URL
    );
    eprint!("Use it ({verb} {})? [y/N] ", path.display());
    let answer = crate::plan::read_line()?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(Some(KeySpec::Login {
            key,
            path,
            destination,
        }));
    }
    Ok(None)
}

/// What preflight found at `keys/<stem>.key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Destination {
    Absent,
    /// A safe file already holding the selected key: nothing to write, but
    /// apply still checks it is the same file.
    Identical(super::ApprovedFile),
    /// `--replace-copied-key`: the approved file (replaced only if it still
    /// is that file) and its complete old contents, for rollback.
    Rotate {
        approved: super::ApprovedFile,
        old: String,
    },
}

/// A copied key file is a token; anything larger is not one — the same
/// bound `validate_key_shape` applies to a key before it is ever written.
const MAX_KEY_BYTES: u64 = crate::registry::auth::MAX_KEY_BYTES as u64;

/// The destination, checked before anything is spent, under the same rules
/// `write_secret_file` / `ensure_private_dir` apply at apply time — so
/// apply cannot refuse what preflight passed. The deepest existing ancestor
/// is validated as a directory chain nobody else can rewrite (the same
/// predicate, nothing created). An existing file must be a regular,
/// singly-linked, private file; it is opened without following links and
/// the *descriptor* is checked for type, links, mode and identity; a body
/// over [`MAX_KEY_BYTES`] or not UTF-8 is refused, never truncated. Only
/// absent, or safe-and-identical, proceeds silently. A safe file with a
/// different key is somebody's — replacing it would rotate every connector
/// that references it — unless `--replace-copied-key` proves, by file
/// identity rather than path spelling, that this stem's connector is its
/// only referent; that inventory fails closed on any unreadable file.
fn preflight_key_destination(
    path: &Path,
    key: &str,
    replace: bool,
    providers_dir: &Path,
    stem: &str,
    owner: Option<super::FileOwner>,
) -> Result<Destination> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let shape = |problem: String| {
        CliError::precondition(format!("{}: {problem}", path.display())).with_fix(
            "the copied login key needs a plain, private, singly-linked file there (or no file) \
             under a directory chain only its owner can write; move whatever is in the way \
             aside, or pass --api-key-file to reference a key of your own",
        )
    };
    // The chain the file hangs from (or will), before the file itself.
    let mut anchor = path.parent();
    while let Some(candidate) = anchor {
        if candidate.exists() || std::fs::symlink_metadata(candidate).is_ok() {
            providers::config::validate_directory(candidate, owner.map(|o| o.uid))
                .map_err(shape)?;
            break;
        }
        anchor = candidate.parent();
    }
    let meta = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Destination::Absent),
        Err(e) => return Err(shape(format!("cannot stat: {e}"))),
        Ok(meta) => meta,
    };
    if meta.file_type().is_symlink() {
        return Err(shape("is a symlink".to_string()));
    }
    if !meta.is_file() {
        return Err(shape("is not a regular file".to_string()));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|e| shape(format!("cannot open: {e}")))?;
    // Everything below is about the descriptor, not the path.
    let opened = file
        .metadata()
        .map_err(|e| shape(format!("cannot stat the opened file: {e}")))?;
    if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err(shape("changed while being checked".to_string()));
    }
    if !opened.is_file() {
        return Err(shape("is not a regular file".to_string()));
    }
    if opened.nlink() != 1 {
        return Err(shape(format!("has {} hard links", opened.nlink())));
    }
    let mode = opened.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(shape(format!("is readable by other users (mode {mode:o})")));
    }
    if let Some(expected) = owner.map(|o| o.uid) {
        if crate::proc::is_root() && opened.uid() != expected {
            return Err(shape(format!(
                "is owned by uid {}, not the inference account (uid {expected})",
                opened.uid()
            )));
        }
    }
    if opened.len() > MAX_KEY_BYTES {
        return Err(shape(format!(
            "is larger than {MAX_KEY_BYTES} bytes; not a key file"
        )));
    }
    let mut raw = Vec::new();
    file.take(MAX_KEY_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|e| shape(format!("cannot read: {e}")))?;
    if raw.len() as u64 > MAX_KEY_BYTES {
        return Err(shape(format!(
            "is larger than {MAX_KEY_BYTES} bytes; not a key file"
        )));
    }
    let approved = super::ApprovedFile::capture(&opened, &raw);
    let existing = String::from_utf8(raw).map_err(|_| shape("is not UTF-8 text".to_string()))?;
    if existing.trim() == key {
        return Ok(Destination::Identical(approved));
    }
    if !replace {
        return Err(CliError::precondition(format!(
            "{} already exists with a different key; refusing to replace it",
            path.display()
        ))
        .with_fix(
            "reference it with --api-key-file, move it aside if it is stale, or pass \
             --replace-copied-key to rotate a key this command copied for this connector",
        ));
    }
    // Rotation: this stem's connector must reference exactly this file (by
    // identity, so `..` and symlinked directories cannot hide a referent),
    // and no other connector may. Anything that cannot be read is a refusal.
    let identity = (opened.dev(), opened.ino());
    let referents = |doc_path: &Path| -> Result<bool> {
        let Some(doc) = super::ProviderFileDoc::load(doc_path)? else {
            return Ok(false);
        };
        let Some(referenced) = doc.value.get("api_key_file").and_then(toml::Value::as_str) else {
            return Ok(false);
        };
        // Only "there is no such file" means "not this key"; any other
        // failure to resolve the reference leaves ownership unproved.
        match std::fs::metadata(referenced) {
            Ok(m) => Ok((m.dev(), m.ino()) == identity),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(CliError::precondition(format!(
                "--replace-copied-key: cannot resolve {referenced} referenced by {}: {e}",
                doc_path.display()
            ))),
        }
    };
    let refuse = |problem: String| {
        CliError::precondition(format!("--replace-copied-key: {problem}"))
            .with_fix("nothing was written or sent; fix what this names and rerun")
    };
    if !referents(&providers_dir.join(format!("{stem}.toml")))? {
        return Err(refuse(format!(
            "{}/{stem}.toml does not reference {}, so it is not a key this command copied \
             for that connector",
            providers_dir.display(),
            path.display()
        )));
    }
    let mut others = Vec::new();
    for sibling in super::ls::provider_files(providers_dir).map_err(refuse)? {
        if sibling.file_stem().is_some_and(|s| s != stem) && referents(&sibling)? {
            others.push(sibling.display().to_string());
        }
    }
    if !others.is_empty() {
        return Err(refuse(format!(
            "{} is also referenced by {}; rotating it would rotate those connectors too",
            path.display(),
            others.join(", ")
        )));
    }
    Ok(Destination::Rotate {
        approved,
        old: existing,
    })
}

/// `<providers root>/keys/<stem>.key`: beside providers.d, where operator
/// key files conventionally live (`/etc/postvec/keys`, `<server-root>/keys`),
/// one per connector file so `univec.toml` and `--name univec-staging`
/// never share (and rotate) one credential.
fn key_path(providers_dir: &Path, stem: &str) -> PathBuf {
    providers_dir
        .parent()
        .unwrap_or(providers_dir)
        .join("keys")
        .join(format!("{stem}.key"))
}

// ---- The best-effort identity check --------------------------------------

/// `GET {base}/v1/registry/index.json` with the key — the unbilled,
/// identity-only route `postvec login` relies on. Best-effort, not a
/// promise: the route is conditionally registered on aphex's side. Only
/// 401/403 is decisive (a bad key, nothing spent); 404, 429, 5xx and
/// transport failures are warnings, and the billed probe decides.
pub async fn identity_check(
    base_url: Option<&str>,
    key: &str,
    timeout: Duration,
    output: &Output,
) -> Result<()> {
    let base = base_url.unwrap_or(providers::univec::DEFAULT_UNIVEC_BASE_URL);
    let url = format!("{}/v1/registry/index.json", base.trim_end_matches('/'));
    let response = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map(|c| c.get(&url).bearer_auth(key))
        .map_err(|e| CliError::internal(format!("http client: {e}")))?
        .send()
        .await;
    match response.map(|r| r.status().as_u16()) {
        Ok(401 | 403) => Err(CliError::precondition(
            "UniVec rejected the key (401/403); nothing was billed",
        )
        .with_fix("check for a typo or a revoked key at https://univec.ai/dashboard/api-keys")),
        Ok(status) if (200..300).contains(&status) => Ok(()),
        Ok(status) => {
            output.note(&format!(
                "identity check at {url} answered {status}; the billed probe decides"
            ));
            Ok(())
        }
        Err(e) => {
            output.note(&format!(
                "identity check at {url} failed ({e}); the billed probe decides"
            ));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(extra: &[&str]) -> ProviderAddArgs {
        let mut argv = vec!["postvec", "provider", "add"];
        argv.extend_from_slice(extra);
        match crate::cli::Cli::parse_from(argv).command {
            crate::cli::Command::Provider(crate::cli::ProviderCommand::Add(args)) => *args,
            _ => unreachable!(),
        }
    }

    fn selectors(extra: &[&str]) -> Result<Selectors> {
        let args = parse(extra);
        let legacy = args.convert_source.is_some();
        let canonical = providers::catalog::canonical_provider(&args.provider_type);
        Selectors::from_args(&args, &canonical, legacy, args.models.len())
    }

    #[test]
    fn selector_combinations_are_validated() {
        // The happy path clap used to reject.
        assert!(selectors(&["univec"]).is_ok());
        assert!(selectors(&["univec", "--model", "a", "--convert-to", "b"]).is_ok());
        let ok = selectors(&["univec", "--convert", "a:b", "--convert", " c : d "]).unwrap();
        assert_eq!(
            ok.convert,
            vec![("a".into(), "b".into()), ("c".into(), "d".into())]
        );

        let err = |extra: &[&str]| {
            selectors(extra)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default()
        };
        assert!(err(&["openai"]).contains("--model is required"));
        assert!(err(&["openai", "--model", "m", "--convert-to", "x"]).contains("univec"));
        assert!(err(&["univec", "--convert", "ab"]).contains("SRC:DST"));
        assert!(err(&["univec", "--convert", ":b"]).contains("SRC:DST"));
        assert!(err(&["univec", "--no-catalog"]).contains("nothing to add"));
        assert!(err(&["univec", "--no-catalog", "--convert-to", "x"]).contains("--no-catalog"));
        assert!(
            err(&["univec", "--converter-name", "n", "--convert-to", "x"])
                .contains("--converter-name")
        );
        assert!(err(&[
            "univec",
            "--converter-name",
            "n",
            "--convert",
            "a:b",
            "--convert",
            "c:d"
        ])
        .contains("--converter-name"));
        assert!(err(&[
            "univec",
            "--model",
            "m",
            "--convert-source",
            "s",
            "--convert-target",
            "t",
            "--source-model",
            "s",
            "--target-model",
            "t",
            "--source-dim",
            "4"
        ])
        .contains("manual converter"));

        assert!(selectors(&["univec"]).unwrap().needs_catalogue(0, false));
        assert!(!selectors(&["univec", "--model", "m"])
            .unwrap()
            .needs_catalogue(1, false));
        assert!(selectors(&["univec", "--all-converters"])
            .unwrap()
            .needs_catalogue(0, false));
    }

    fn catalogue() -> Vec<ListedModel> {
        let c = |s: &str, sd: u32, t: &str, td: u32| ListedModel {
            provider_model_id: t.into(),
            kind: ListedKind::Convert,
            dim: td,
            space: Some(t.into()),
            source: Some((s.into(), sd)),
            sequence_len: None,
            quality: None,
        };
        vec![
            ListedModel {
                provider_model_id: "t".into(),
                kind: ListedKind::Embed,
                dim: 4,
                space: Some("t".into()),
                source: None,
                sequence_len: Some(256),
                quality: None,
            },
            c("a", 8, "t", 4),
            c("b", 8, "t", 4),
            c("a", 8, "u", 2),
        ]
    }

    #[test]
    fn selection_over_a_catalogue_is_a_union_and_an_empty_one_is_refused() {
        let cat = catalogue();
        let into_t = selectors(&["univec", "--convert-to", "t"])
            .unwrap()
            .converters(&cat)
            .unwrap();
        assert_eq!(into_t.len(), 2);
        let from_a = selectors(&["univec", "--convert-from", "a", "--convert", "b:t"])
            .unwrap()
            .converters(&cat)
            .unwrap();
        assert_eq!(from_a.len(), 3, "union, deduplicated");
        assert_eq!(
            selectors(&["univec", "--all-converters", "--convert-to", "t"])
                .unwrap()
                .converters(&cat)
                .unwrap()
                .len(),
            3
        );

        let err = selectors(&["univec", "--convert-to", "nope"])
            .unwrap()
            .converters(&cat)
            .unwrap_err()
            .to_string();
        assert!(err.contains("targets are: t, u"), "{err}");
        let err = selectors(&["univec", "--convert", "x:t"])
            .unwrap()
            .converters(&cat)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no converter x -> t"), "{err}");
    }

    #[test]
    fn a_catalogue_converter_becomes_a_two_vocabulary_entry() {
        let cat = catalogue();
        let m = converter_model(&cat[1], &cat, None).unwrap();
        assert_eq!(m.public_name, "univec-convert-a-to-t");
        assert_eq!((m.dim, m.id.as_str()), (Some(4), "t"));
        let c = m.convert.unwrap();
        assert_eq!(
            (
                c.provider_source_id.as_str(),
                c.source_model.as_str(),
                c.target_model.as_str(),
                c.source_dim
            ),
            ("a", "a", "t", 8)
        );
        assert!(m.catalogued);
        // A stated width that contradicts the listed embed is refused.
        let mut bad = cat[1].clone();
        bad.dim = 5;
        let err = converter_model(&bad, &cat, None)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("4-dimensional"), "{err}");
    }

    #[test]
    fn the_login_key_lands_beside_providers_d_one_per_stem() {
        assert_eq!(
            key_path(Path::new("/etc/postvec/providers.d"), "univec"),
            PathBuf::from("/etc/postvec/keys/univec.key")
        );
        assert_eq!(
            key_path(Path::new("/opt/postvec/providers.d"), "univec-staging"),
            PathBuf::from("/opt/postvec/keys/univec-staging.key")
        );
    }

    /// Precedence: the environment wins; equal stores agree; different
    /// stores are an ambiguity, never a silent pick.
    #[test]
    fn credential_precedence_is_env_then_agreement_then_ambiguity() {
        let env = crate::registry::auth::API_KEY_ENV.to_string();
        let k = |key: &str, source: &str| (key.to_string(), source.to_string());
        assert!(matches!(choose(vec![]), Choice::None));
        match choose(vec![
            k("uv_root_key_00000", "the credential store"),
            k("uv_env_key_000000", &env),
        ]) {
            Choice::One(key, source) => {
                assert_eq!((key.as_str(), source), ("uv_env_key_000000", env.clone()))
            }
            _ => panic!("env must win"),
        }
        match choose(vec![
            k("uv_same_key_00000", "the credential store"),
            k("uv_same_key_00000", "amx's postvec login"),
        ]) {
            Choice::One(key, source) => {
                assert_eq!(key, "uv_same_key_00000");
                assert!(source.contains("and"), "{source}");
            }
            _ => panic!("equal keys agree"),
        }
        match choose(vec![
            k("uv_root_key_00000", "the credential store"),
            k("uv_user_key_00000", "amx's postvec login"),
        ]) {
            Choice::Ambiguous(choices) => assert_eq!(choices.len(), 2),
            _ => panic!("different keys are ambiguous"),
        }
    }

    /// Every unsafe destination shape — and an unsafe chain above an absent
    /// destination — is refused before anything is spent; only absent, or
    /// safe-and-identical, proceeds. Rotation inventory fails closed.
    #[test]
    fn the_key_destination_is_preflighted_for_shape_chain_and_content() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let providers = dir.path().join("providers.d");
        std::fs::create_dir(&providers).unwrap();
        std::fs::set_permissions(&providers, std::fs::Permissions::from_mode(0o700)).unwrap();
        let keys = dir.path().join("keys");
        let path = keys.join("univec.key");
        let key = "uv_new_key_0000000";
        let check = |replace: bool| {
            preflight_key_destination(&path, key, replace, &providers, "univec", None)
        };
        let refused = |what: &str| {
            let err = check(false)
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            assert!(err.contains(what), "expected {what:?} in {err:?}");
        };
        let not_root = unsafe { libc::geteuid() } != 0;

        // The chain above an ABSENT destination.
        assert_eq!(check(false).unwrap(), Destination::Absent);
        std::fs::write(&keys, "a file where keys/ should be").unwrap();
        refused("not a directory");
        std::fs::remove_file(&keys).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &keys).unwrap();
        refused("symlink");
        std::fs::remove_file(&keys).unwrap();
        std::fs::create_dir(&keys).unwrap();
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o777)).unwrap();
        refused("writable");
        std::fs::set_permissions(&keys, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(check(false).unwrap(), Destination::Absent);

        // The destination itself.
        let reset = || {
            let _ = std::fs::remove_dir_all(&path);
            let _ = std::fs::remove_file(&path);
        };
        std::fs::create_dir(&path).unwrap();
        refused("is not a regular file");
        reset();
        let real = keys.join("real.key");
        std::fs::write(&real, key).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        refused("symlink");
        reset();
        std::fs::hard_link(&real, &path).unwrap();
        refused("hard links");
        reset();
        std::fs::remove_file(&real).unwrap();
        std::fs::write(&path, [0xff, 0xfe, b'x']).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        refused("not UTF-8");
        std::fs::write(&path, "x".repeat(16 * 1024 + 1)).unwrap();
        refused("larger than");
        std::fs::write(&path, key).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        refused("readable by other users");
        if not_root {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            refused("cannot open");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(&path, format!("{key}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(matches!(check(false).unwrap(), Destination::Identical(_)));

        // A different key: refused without --replace-copied-key, and the
        // rotation inventory must prove sole ownership by identity.
        std::fs::write(&path, "uv_other_key_00000").unwrap();
        let err = check(false).unwrap_err().to_string();
        assert!(err.contains("different key"), "{err}");
        let err = check(true).unwrap_err().to_string();
        assert!(err.contains("does not reference"), "{err}");
        let toml = |stem: &str, referenced: &str| {
            let file = providers.join(format!("{stem}.toml"));
            std::fs::write(
                &file,
                format!("provider = \"univec\"\napi_key_file = \"{referenced}\"\n"),
            )
            .unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            file
        };
        toml("univec", &path.display().to_string());
        assert!(
            matches!(check(true).unwrap(), Destination::Rotate { old, .. } if old == "uv_other_key_00000")
        );
        // A sibling naming the same file through `..` is still a referent.
        let alias = format!("{}/../keys/univec.key", providers.display());
        let staging = toml("univec-staging", &alias);
        let err = check(true).unwrap_err().to_string();
        assert!(
            err.contains("also referenced by") && err.contains("univec-staging"),
            "{err}"
        );
        // A sibling whose reference cannot be RESOLVED (not merely absent)
        // is a refusal too: ownership was not proved.
        if not_root {
            let hidden = dir.path().join("hidden");
            std::fs::create_dir(&hidden).unwrap();
            let staging2 = toml("univec-other", &hidden.join("k.key").display().to_string());
            std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
            let err = check(true).unwrap_err().to_string();
            std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_file(&staging2).unwrap();
            assert!(
                err.contains("cannot resolve") && err.contains("univec-other"),
                "{err}"
            );
        }
        // An unreadable or malformed sibling is a refusal, not "no sibling".
        std::fs::write(&staging, "provider = [[[").unwrap();
        let err = check(true).unwrap_err().to_string();
        assert!(
            err.contains("univec-staging") && err.contains("parse"),
            "{err}"
        );
        std::fs::remove_file(&staging).unwrap();
        if not_root {
            std::fs::set_permissions(&providers, std::fs::Permissions::from_mode(0o000)).unwrap();
            let err = check(true).unwrap_err().to_string();
            std::fs::set_permissions(&providers, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(err.contains("scan") || err.contains("cannot"), "{err}");
        }
    }
}
