//! What `provider add univec` knows that no other connector needs: the
//! public catalogue (`GET /v1/models`), the selectors over it, the offer to
//! reuse the `postvec login` key, and the free identity check that runs
//! before any billed probe.

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
    })
}

// ---- The login-key offer --------------------------------------------------

/// A stored UniVec credential this run could copy: `POSTVEC_API_KEY`, the
/// effective user's store, or — under `sudo` — the invoking user's own
/// store, read as root from that user's home (`sudo` resets `$HOME`).
fn stored_credential(output: &Output) -> Option<(String, String)> {
    use crate::registry::auth;
    match auth::resolve(None) {
        Ok(Some(c)) => return Some((c.key, c.source.to_string())),
        Ok(None) => {}
        Err(e) => output.note(&format!("stored credential not usable: {e}")),
    }
    if !crate::proc::is_root() {
        return None;
    }
    let user = std::env::var("SUDO_USER")
        .ok()
        .filter(|u| !u.is_empty() && u != "root")?;
    let account = crate::proc::OsAccount::lookup(&user).ok()?;
    let home = crate::proc::home_dir(&user).ok()?;
    let store = home.join(".config/postvec/auth.json");
    match auth::read_private_store(&store, account.uid) {
        Ok(Some(key)) => Some((key, format!("{}'s postvec login", user))),
        Ok(None) => None,
        Err(e) => {
            output.note(&format!("{}: {e}", store.display()));
            None
        }
    }
}

/// The offer, or the scripted opt-in. `None` falls through to the ordinary
/// key sources. Never implicit: an interactive run asks (default no), a
/// non-interactive one needs `--api-key-from-login`.
pub(super) fn login_key_offer(
    args: &ProviderAddArgs,
    providers_dir: &Path,
    output: &Output,
) -> Result<Option<KeySpec>> {
    let path = key_path(providers_dir);
    if args.api_key_from_login {
        let (key, source) = stored_credential(output).ok_or_else(|| {
            CliError::usage(
                "--api-key-from-login: no stored credential (POSTVEC_API_KEY, or `postvec login`)",
            )
        })?;
        output.note(&format!(
            "using the key from {source}; copied to {}",
            path.display()
        ));
        return Ok(Some(KeySpec::Login { key, path }));
    }
    if !crate::proc::is_stdin_tty() || output.is_json() {
        return Ok(None);
    }
    let Some((key, source)) = stored_credential(output) else {
        return Ok(None);
    };
    eprintln!(
        "Found a UniVec key from {source} ({}). Use it for hosted inference too?\n\
         It will be billed from this host. A key created with a $0 spending limit (the login \
         prompt's advice for downloads) fails with 402; mint a separate inference key at \
         {} if so.",
        crate::registry::auth::masked_key(&key),
        crate::registry::urls::DASHBOARD_URL
    );
    eprint!("Copy it to {}? [y/N] ", path.display());
    let answer = crate::plan::read_line()?;
    if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(Some(KeySpec::Login { key, path }));
    }
    Ok(None)
}

/// `<providers root>/keys/univec.key`: beside providers.d, where operator
/// key files conventionally live (`/etc/postvec/keys`, `<server-root>/keys`).
fn key_path(providers_dir: &Path) -> PathBuf {
    providers_dir
        .parent()
        .unwrap_or(providers_dir)
        .join("keys")
        .join("univec.key")
}

// ---- The free identity check ----------------------------------------------

/// `GET {base}/v1/registry/index.json` with the key — the same unbilled,
/// identity-only proof `postvec login` relies on. 401/403 is a bad key and
/// nothing has been spent; anything else (outage, a front without the
/// route) is a warning, and the billed probe decides.
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
            source: Some((s.into(), sd)),
            sequence_len: None,
            quality: None,
        };
        vec![
            ListedModel {
                provider_model_id: "t".into(),
                kind: ListedKind::Embed,
                dim: 4,
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
    fn the_login_key_lands_beside_providers_d() {
        assert_eq!(
            key_path(Path::new("/etc/postvec/providers.d")),
            PathBuf::from("/etc/postvec/keys/univec.key")
        );
        assert_eq!(
            key_path(Path::new("/var/lib/postvec-server/providers.d")),
            PathBuf::from("/var/lib/postvec-server/keys/univec.key")
        );
    }
}
