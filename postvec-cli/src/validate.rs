//! Validation and normalization of every operator-supplied value.
//!
//! Two invariants this module exists to hold:
//!
//! 1. Nothing that reaches a PostgreSQL configuration file can terminate a
//!    quoted literal or start a new line (that would let a database name
//!    inject an arbitrary setting).
//! 2. Nothing that reaches SQL is interpolated except through
//!    [`quote_identifier`], and only where PostgreSQL cannot parameterize
//!    (`CREATE DATABASE`).

use crate::error::{redact, CliError, Result};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

/// PostgreSQL's NAMEDATALEN - 1. Longer names are silently truncated by the
/// server, which would make the CLI's own bookkeeping disagree with reality.
const MAX_IDENTIFIER_BYTES: usize = 63;

/// Characters that must never appear in a value rendered into a PostgreSQL
/// configuration file or a comma-separated GUC list.
fn reject_unsafe_characters(value: &str, what: &str) -> Result<()> {
    if let Some(bad) = value.chars().find(|c| matches!(c, '\0' | '\n' | '\r')) {
        return Err(CliError::usage(format!(
            "{what} contains an illegal character ({:?}); NUL, CR and LF are rejected",
            bad
        )));
    }
    Ok(())
}

/// A database name that is safe to render into `postvec.database` and to
/// quote as an SQL identifier.
pub fn database_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(CliError::usage("database name is empty"));
    }
    reject_unsafe_characters(name, "database name")?;
    if name.contains(',') {
        return Err(CliError::usage(format!(
            "database name {name:?} contains a comma; postvec.database is a comma-separated \
             list and cannot represent it"
        )));
    }
    if name.len() > MAX_IDENTIFIER_BYTES {
        return Err(CliError::usage(format!(
            "database name is {} bytes; PostgreSQL truncates at {MAX_IDENTIFIER_BYTES}",
            name.len()
        )));
    }
    Ok(name.to_string())
}

/// Validate, deduplicate and sort a `--database` list. Sorted because the
/// rendered `postvec.database` must be byte-identical across reruns; database
/// order carries no meaning for the launcher.
pub fn database_list(raw: &[String]) -> Result<Vec<String>> {
    let list = database_list_allow_empty(raw)?;
    if list.is_empty() {
        return Err(CliError::usage("--database is required"));
    }
    Ok(list)
}

pub fn database_list_allow_empty(raw: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(raw.len());
    for item in raw {
        let name = database_name(item)?;
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out.sort();
    Ok(out)
}

/// Model names for `postvec.embedded_models`: same list constraints as
/// database names, sorted and deduplicated for stable rendering.
pub fn model_list(raw: &[String]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(raw.len());
    for item in raw {
        let name = item.trim();
        if name.is_empty() {
            return Err(CliError::usage("--model is empty"));
        }
        reject_unsafe_characters(name, "model name")?;
        if name.contains(',') {
            return Err(CliError::usage(format!(
                "model name {name:?} contains a comma; postvec.embedded_models is a \
                 comma-separated list and cannot represent it"
            )));
        }
        if !out.iter().any(|m| m == name) {
            out.push(name.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// An inference gRPC endpoint. The hostname is preserved verbatim for the
/// configuration file; resolution happens only for probing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GrpcEndpoint {
    /// Exactly what goes into the GUC.
    pub authority: String,
    pub host: String,
    pub port: u16,
}

impl std::fmt::Display for GrpcEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.authority)
    }
}

/// Parse `host:port` or `[ipv6]:port`. Rejects anything with a scheme, path,
/// query, fragment or userinfo — the extension's gRPC client dials a bare
/// authority and would fail confusingly on those.
pub fn grpc_endpoint(raw: &str) -> Result<GrpcEndpoint> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(CliError::usage("--grpc endpoint is empty"));
    }
    reject_unsafe_characters(value, "gRPC endpoint")?;
    // The value is redacted in the message: an operator who pastes a URI with
    // userinfo into --grpc must not have it echoed back into their terminal,
    // shell history or CI log.
    let shown = redact(value);
    let bad = move |why: &str| {
        CliError::usage(format!(
            "invalid --grpc endpoint {shown:?}: {why} (expected host:port or [ipv6]:port)"
        ))
    };
    if value.contains("://") {
        return Err(bad("gRPC endpoints carry no scheme"));
    }
    if value.contains('@') {
        return Err(bad("userinfo is not accepted"));
    }
    if value.contains('/') || value.contains('?') || value.contains('#') {
        return Err(bad("no path, query or fragment is accepted"));
    }

    let (host, port_str) = if let Some(rest) = value.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| bad("unclosed '['"))?;
        let port = tail
            .strip_prefix(':')
            .ok_or_else(|| bad("missing :port after ']'"))?;
        // Reject a bracketed name that is not actually an IPv6 literal.
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(bad("bracketed host must be an IPv6 literal"));
        }
        (host, port)
    } else {
        let (host, port) = value.rsplit_once(':').ok_or_else(|| bad("missing :port"))?;
        if host.contains(':') {
            return Err(bad("IPv6 literals must be bracketed"));
        }
        (host, port)
    };

    if host.is_empty() {
        return Err(bad("empty host"));
    }
    let port: u16 = port_str
        .parse()
        .map_err(|_| bad("port must be 1-65535"))
        .and_then(|p: u16| if p == 0 { Err(bad("port 0")) } else { Ok(p) })?;

    Ok(GrpcEndpoint {
        authority: value.to_string(),
        host: host.to_string(),
        port,
    })
}

/// Validate a `--grpc` list. Order is preserved: it drives the extension's
/// round-robin and failover order. Exact duplicates are dropped.
pub fn grpc_endpoints(raw: &[String]) -> Result<Vec<GrpcEndpoint>> {
    let mut out: Vec<GrpcEndpoint> = Vec::with_capacity(raw.len());
    for item in raw {
        let ep = grpc_endpoint(item)?;
        if !out.iter().any(|e| e.authority == ep.authority) {
            out.push(ep);
        }
    }
    if out.is_empty() {
        return Err(CliError::usage(
            "at least one --grpc endpoint is required in remote mode",
        ));
    }
    Ok(out)
}

/// An inference HTTP base URL used for `GET /config`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HttpEndpoint {
    /// Exactly what goes into the GUC (no trailing slash).
    pub base: String,
    pub is_https: bool,
}

impl HttpEndpoint {
    /// `<base>/config`, joined exactly once.
    pub fn config_url(&self) -> String {
        format!("{}/config", self.base)
    }

    /// `<base>/health`.
    pub fn health_url(&self) -> String {
        format!("{}/health", self.base)
    }
}

impl std::fmt::Display for HttpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.base)
    }
}

/// Parse an HTTP(S) base URL. Credentials are rejected rather than redacted:
/// this value gets persisted to a configuration file readable by the
/// PostgreSQL account, so it must never carry a secret in the first place.
pub fn http_endpoint(raw: &str) -> Result<HttpEndpoint> {
    let value = raw.trim();
    if value.is_empty() {
        return Err(CliError::usage("--http endpoint is empty"));
    }
    reject_unsafe_characters(value, "HTTP endpoint")?;
    // Redacted for the same reason as above: this message is the one an
    // operator sees when they paste a URL containing a password.
    let shown = redact(value);
    let bad = move |why: &str| {
        CliError::usage(format!(
            "invalid --http endpoint {shown:?}: {why} \
             (expected http(s)://host[:port] with no credentials or query)"
        ))
    };
    let url = url::Url::parse(value).map_err(|e| bad(&e.to_string()))?;
    match url.scheme() {
        "http" | "https" => {}
        other => return Err(bad(&format!("unsupported scheme {other:?}"))),
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("credentials in the URL are not accepted"));
    }
    if url.query().is_some() {
        return Err(bad("query strings are not accepted"));
    }
    if url.fragment().is_some() {
        return Err(bad("fragments are not accepted"));
    }
    if url.host_str().unwrap_or_default().is_empty() {
        return Err(bad("empty host"));
    }
    // Keep any base path the operator supplied (a reverse proxy may mount
    // the inference host under a prefix), but normalize the trailing slash so
    // `/config` is joined exactly once.
    let base = url.as_str().trim_end_matches('/').to_string();
    Ok(HttpEndpoint {
        is_https: url.scheme() == "https",
        base,
    })
}

pub fn http_endpoints(raw: &[String]) -> Result<Vec<HttpEndpoint>> {
    let mut out: Vec<HttpEndpoint> = Vec::with_capacity(raw.len());
    for item in raw {
        let ep = http_endpoint(item)?;
        if !out.iter().any(|e| e.base == ep.base) {
            out.push(ep);
        }
    }
    if out.is_empty() {
        return Err(CliError::usage(
            "at least one --http endpoint is required in remote mode",
        ));
    }
    Ok(out)
}

/// The embedded listeners are unauthenticated: anything that can reach them
/// can drive inference on the database host.
pub fn loopback_listener(addr: SocketAddr, flag: &str) -> Result<()> {
    if !addr.ip().is_loopback() {
        return Err(CliError::usage(format!(
            "{flag} {addr} is not a loopback address; the embedded listeners have no \
             authentication and must not be exposed"
        )));
    }
    if addr.port() == 0 {
        return Err(CliError::usage(format!("{flag} port 0 is not usable")));
    }
    Ok(())
}

/// Absoluteness and character safety only. Existence and readability are
/// checked later, from the PostgreSQL account's point of view.
pub fn absolute_path(path: &Path, flag: &str) -> Result<PathBuf> {
    let display = path.display().to_string();
    reject_unsafe_characters(&display, flag)?;
    if !path.is_absolute() {
        return Err(CliError::usage(format!(
            "{flag} must be an absolute path (got {display:?}); the PostgreSQL server \
             resolves it from its own working directory"
        )));
    }
    // Normalize away trailing slashes and `.` components without touching the
    // filesystem (canonicalize would resolve symlinks we may not be able to
    // read yet, and would fail for a not-yet-created root).
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Render a value as a PostgreSQL **configuration** string literal.
///
/// This is not SQL quoting: postgresql.conf uses single quotes with `''` as
/// the escape and has no other escape mechanism, which is why control
/// characters must be rejected outright rather than escaped.
pub fn config_literal(value: &str) -> Result<String> {
    reject_unsafe_characters(value, "configuration value")?;
    Ok(format!("'{}'", value.replace('\'', "''")))
}

/// Render an SQL identifier. The only place the CLI interpolates into SQL:
/// `CREATE DATABASE` cannot be parameterized and cannot run in a transaction.
pub fn quote_identifier(value: &str) -> Result<String> {
    if value.contains('\0') {
        return Err(CliError::usage("identifier contains NUL"));
    }
    Ok(format!("\"{}\"", value.replace('"', "\"\"")))
}

/// Parse a pgvector version as reported by `pg_available_extensions`.
/// PostgreSQL packages append suffixes semver rejects (`0.8.0-1.pgdg`,
/// `0.8`), so normalize before parsing.
pub fn parse_extension_version(raw: &str) -> Option<semver::Version> {
    let core = raw.trim();
    let core = core.split(['-', '+']).next()?;
    let mut parts = core
        .split('.')
        .map(|p| p.trim_start_matches(|c: char| !c.is_ascii_digit()))
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        });
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or_default().parse().unwrap_or(0);
    let patch = parts.next().unwrap_or_default().parse().unwrap_or(0);
    Some(semver::Version::new(major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_names_reject_list_and_control_characters() {
        assert_eq!(database_name(" univec ").unwrap(), "univec");
        assert!(database_name("").is_err());
        assert!(database_name("a,b").is_err());
        assert!(database_name("a\nb").is_err());
        assert!(database_name("a\rb").is_err());
        assert!(database_name("a\0b").is_err());
        assert!(database_name(&"x".repeat(64)).is_err());
        assert!(database_name(&"x".repeat(63)).is_ok());
    }

    #[test]
    fn database_lists_sort_and_deduplicate() {
        let list = database_list(&["b".into(), "a".into(), "b".into()]).unwrap();
        assert_eq!(list, ["a", "b"]);
        assert!(database_list(&[]).is_err());
    }

    #[test]
    fn grpc_endpoints_accept_authority_forms() {
        assert_eq!(grpc_endpoint("192.0.2.2:33333").unwrap().port, 33333);
        assert_eq!(
            grpc_endpoint("nin-1.example:33333").unwrap().host,
            "nin-1.example"
        );
        let v6 = grpc_endpoint("[fd00::1]:33333").unwrap();
        assert_eq!(v6.host, "fd00::1");
        assert_eq!(v6.authority, "[fd00::1]:33333");
    }

    #[test]
    fn grpc_endpoints_reject_non_authority_input() {
        for bad in [
            "http://192.0.2.2:33333",
            "grpc://h:1",
            "user@h:1",
            "h:1/path",
            "h:1?x=1",
            "h:0",
            "h:70000",
            "h",
            ":33333",
            "fd00::1:33333",
            "[fd00::1:33333",
            "[nin-1.example]:33333",
        ] {
            assert!(grpc_endpoint(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn grpc_list_preserves_order_and_drops_duplicates() {
        let list = grpc_endpoints(&["b:2".into(), "a:1".into(), "b:2".into()]).unwrap();
        assert_eq!(
            list.iter()
                .map(|e| e.authority.as_str())
                .collect::<Vec<_>>(),
            ["b:2", "a:1"],
            "endpoint order is operationally meaningful"
        );
    }

    #[test]
    fn http_endpoints_normalize_trailing_slash() {
        let ep = http_endpoint("https://192.0.2.2:22222/").unwrap();
        assert_eq!(ep.base, "https://192.0.2.2:22222");
        assert_eq!(ep.config_url(), "https://192.0.2.2:22222/config");
        assert!(ep.is_https);
        let plain = http_endpoint("http://nin-1.example:22222").unwrap();
        assert!(!plain.is_https);
        // A proxy prefix survives, joined exactly once.
        assert_eq!(
            http_endpoint("https://h/nin/").unwrap().config_url(),
            "https://h/nin/config"
        );
    }

    /// The rejection message is the most likely place for a pasted password to
    /// end up in a terminal, a shell history or a CI log.
    #[test]
    fn rejection_messages_do_not_echo_credentials() {
        let http = http_endpoint("https://alice:s3cret@192.0.2.2:22222").unwrap_err();
        assert!(http.to_string().contains("credentials"));
        assert!(!http.to_string().contains("s3cret"), "{http}");

        let grpc = grpc_endpoint("postgres://alice:s3cret@192.0.2.2:33333").unwrap_err();
        assert!(!grpc.to_string().contains("s3cret"), "{grpc}");
    }

    #[test]
    fn http_endpoints_reject_credentials_and_schemes() {
        for bad in [
            "ftp://h",
            "https://u:p@h",
            "https://u@h",
            "https://h?x=1",
            "https://h#f",
            "192.0.2.2:22222",
            "",
        ] {
            assert!(http_endpoint(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn config_literals_double_single_quotes() {
        assert_eq!(config_literal("univec").unwrap(), "'univec'");
        assert_eq!(config_literal("o'brien").unwrap(), "'o''brien'");
        assert!(config_literal("a\nb").is_err());
    }

    /// The property that matters: a rendered literal is exactly one line and
    /// its quoting cannot be escaped, whatever the input.
    #[test]
    fn config_literals_never_break_out() {
        for value in [
            "a'b",
            "a''b",
            "'",
            "''",
            "a' \n shared_preload_libraries='evil",
            "x'; DROP",
        ] {
            match config_literal(value) {
                Ok(rendered) => {
                    assert!(rendered.starts_with('\'') && rendered.ends_with('\''));
                    assert!(!rendered.contains('\n'));
                    let inner = &rendered[1..rendered.len() - 1];
                    // Every quote inside is part of a doubled pair.
                    let mut chars = inner.chars().peekable();
                    while let Some(c) = chars.next() {
                        if c == '\'' {
                            assert_eq!(chars.next(), Some('\''), "unpaired quote in {rendered}");
                        }
                    }
                }
                Err(_) => assert!(value.contains('\n')),
            }
        }
    }

    #[test]
    fn identifiers_double_quotes() {
        assert_eq!(quote_identifier("univec").unwrap(), "\"univec\"");
        assert_eq!(quote_identifier("we\"ird").unwrap(), "\"we\"\"ird\"");
        assert!(quote_identifier("a\0b").is_err());
    }

    #[test]
    fn extension_versions_tolerate_package_suffixes() {
        let v = |s: &str| parse_extension_version(s).unwrap();
        assert_eq!(v("0.8.0"), semver::Version::new(0, 8, 0));
        assert_eq!(v("0.8"), semver::Version::new(0, 8, 0));
        assert_eq!(v("0.8.5-1.pgdg22.04+1"), semver::Version::new(0, 8, 5));
        assert_eq!(v("1.0.0"), semver::Version::new(1, 0, 0));
        assert!(v("0.8.0") >= semver::Version::new(0, 8, 0));
        assert!(v("0.7.4") < semver::Version::new(0, 8, 0));
        assert!(parse_extension_version("").is_none());
        assert!(parse_extension_version("unreleased").is_none());
    }

    #[test]
    fn absolute_paths_are_required_and_normalized() {
        assert_eq!(
            absolute_path(Path::new("/opt/./engine/"), "--path").unwrap(),
            PathBuf::from("/opt/engine")
        );
        assert!(absolute_path(Path::new("relative/path"), "--path").is_err());
    }
}
