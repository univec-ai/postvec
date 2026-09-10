// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Settings resolution: `defaults < file < environment < flags`.
//!
//! An absent flag leaves a file value in place. An unknown file key is
//! fatal. A node can start with no file at all.

use crate::cli::{
    ServeArgs, DEFAULT_ADMIN_PORT, DEFAULT_GOSSIP_PORT, DEFAULT_GRPC_PORT, DEFAULT_HTTP_PORT,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_GROUP: &str = "postvec";
pub const DEFAULT_PREDICT_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_MAX_RESIDENT_MODELS: usize = 16;
/// How long a shutdown keeps serving while `/ready` already answers 503.
///
/// Five seconds covers the usual one-to-five-second probe interval, so a
/// load balancer sees the 503 before the listener closes.
pub const DEFAULT_DRAIN_DELAY_MS: u64 = 5_000;
/// Floor and ceiling for the autodetected `--max-inflight`.
///
/// The value bounds concurrently executing predictions: each ONNX session
/// runs its own intra-op pool, and each in-flight request may hold a
/// response tree up to the envelope in [`crate::limits`]. A dedicated node
/// uses its hardware, with this ceiling.
pub const MIN_AUTO_INFLIGHT: usize = 4;
pub const MAX_AUTO_INFLIGHT: usize = 16;
/// Floor on the execution budget. Below this, no real model can answer.
pub const MIN_PREDICT_TIMEOUT_MS: u64 = 100;

const CONFIG_FILENAME: &str = "postvec-server.json";

/// Environment lookup, abstracted so the precedence tests stay independent
/// under `cargo test`'s thread pool.
pub trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        match std::env::var(key) {
            Ok(v) if !v.trim().is_empty() => Some(v),
            _ => None,
        }
    }
}

impl EnvSource for BTreeMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        BTreeMap::get(self, key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
}

// ---- The file layer ----------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SslFile {
    pub cert: Option<String>,
    #[serde(alias = "cert_key")]
    pub key: Option<String>,
}

/// The optional `postvec-server.json`. Every field is optional; unknown
/// fields are refused. There is no `hub` block: this process loads models
/// from disk.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub managed: Option<Vec<crate::managed::ManagedDb>>,
    pub bind_address: Option<String>,
    pub http_port: Option<u16>,
    pub grpc_port: Option<u16>,
    pub gossip_port: Option<u16>,
    pub admin_port: Option<u16>,
    /// `cluster` is accepted; `peers` is the documented name.
    #[serde(alias = "cluster")]
    pub peers: Option<Vec<String>>,
    pub group: Option<String>,
    pub advertise: Option<String>,
    pub frontend: Option<String>,
    pub ssl: Option<SslFile>,
    pub insecure: Option<bool>,
    pub models: Option<Vec<String>>,
    /// providers.d directory for external embedding providers.
    pub providers_path: Option<String>,
    pub predict_timeout_ms: Option<u64>,
    pub max_inflight: Option<usize>,
    pub max_resident_models: Option<usize>,
    pub drain_delay_ms: Option<u64>,
    pub warmup: Option<bool>,
    pub metrics: Option<bool>,
    pub log_level: Option<String>,
    /// Serve the registry's pull/activate/deactivate routes on the public
    /// port as well as on the loopback admin port.
    pub manage: Option<bool>,
    /// Directory of a built dashboard (`index.html` + assets). Optional.
    pub web_ui: Option<String>,
}

/// Strip comment keys, recursively.
///
/// Any key beginning with `//` is ignored. The shipped example file uses
/// this so an operator can annotate the configuration in place.
fn strip_comments(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, _| !key.starts_with("//"));
            for nested in map.values_mut() {
                strip_comments(nested);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_comments),
        _ => {}
    }
}

impl FileConfig {
    pub fn parse(raw: &str, origin: &Path) -> Result<Self, String> {
        let mut value: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("{}: {e}", origin.display()))?;
        strip_comments(&mut value);
        serde_json::from_value(value).map_err(|e| {
            format!(
                "{}: {e}\n\
                 (unknown keys are refused on purpose — a typo that is ignored produces a \
                 node that starts and then behaves nothing like its configuration. Keys \
                 beginning with // are treated as comments and ignored.)",
                origin.display()
            )
        })
    }
}

// ---- The resolved settings --------------------------------------------

#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub managed: Vec<crate::managed::ManagedDb>,
    pub root: PathBuf,
    /// The file that was actually read, for the boot log. `None` = none found.
    pub config_path: Option<PathBuf>,
    pub bind: IpAddr,
    pub http_port: u16,
    pub grpc_port: u16,
    pub gossip_port: u16,
    pub admin_port: u16,
    pub peers: Vec<String>,
    pub group: String,
    /// Operator-pinned advertise IP. `None` means "derive it"; the derivation
    /// lives in [`crate::net`] because it can touch the network stack.
    pub advertise: Option<IpAddr>,
    pub frontend: Option<String>,
    /// `None` = `--insecure`: discovery is served over plain HTTP.
    pub tls: Option<TlsPaths>,
    pub models: Vec<String>,
    /// providers.d directory (default `<root>/providers.d`). Missing or
    /// empty means no provider-backed models.
    pub providers_path: PathBuf,
    pub predict_timeout: Duration,
    pub max_inflight: usize,
    pub max_resident_models: usize,
    pub drain_delay: Duration,
    pub warmup: bool,
    pub metrics: bool,
    pub log_level: String,
    pub manage: bool,
    /// Built SPA directory. `None` means "search the default locations".
    pub web_ui: Option<PathBuf>,
}

/// The default for `--max-inflight`: this machine's parallelism, clamped.
pub fn default_max_inflight() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(MIN_AUTO_INFLIGHT)
        .clamp(MIN_AUTO_INFLIGHT, MAX_AUTO_INFLIGHT)
}

/// Engine root the packages install to and the postvec CLI manages by
/// default. One path on every host, whether the engine is embedded, remote
/// or both.
pub const DEFAULT_ROOT: &str = "/opt/postvec";

/// `--root` > `POSTVEC_SERVER_ROOT` > [`DEFAULT_ROOT`].
///
/// The on-disk layout matches embedded mode, so a root is portable between
/// an in-database engine and this server.
pub fn resolve_root(flag: Option<&Path>, env: &dyn EnvSource) -> Result<PathBuf, String> {
    Ok(flag
        .map(|p| p.to_path_buf())
        .or_else(|| env.get("POSTVEC_SERVER_ROOT").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT)))
}

/// Where to look for the optional configuration file.
///
/// An explicit path that is missing is an error. An implicit candidate that
/// is missing means no file.
pub fn resolve_config_path(
    flag: Option<&Path>,
    env: &dyn EnvSource,
    root: &Path,
) -> Result<Option<PathBuf>, String> {
    if let Some(explicit) = flag
        .map(|p| p.to_path_buf())
        .or_else(|| env.get("POSTVEC_SERVER_CONFIG").map(PathBuf::from))
    {
        return if explicit.is_file() {
            Ok(Some(explicit))
        } else {
            Err(format!(
                "configuration file {} does not exist (it was requested explicitly, so this \
                 is an error rather than a fallback to the defaults)",
                explicit.display()
            ))
        };
    }
    for candidate in [
        root.join(CONFIG_FILENAME),
        PathBuf::from("/etc/postvec-server/config.json"),
        PathBuf::from("/etc/postvec-server.json"),
        PathBuf::from(CONFIG_FILENAME),
    ] {
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn env_u16(env: &dyn EnvSource, key: &str) -> Result<Option<u16>, String> {
    env.get(key)
        .map(|v| {
            v.trim()
                .parse::<u16>()
                .map_err(|e| format!("{key}={v:?}: {e}"))
        })
        .transpose()
}

fn env_u64(env: &dyn EnvSource, key: &str) -> Result<Option<u64>, String> {
    env.get(key)
        .map(|v| {
            v.trim()
                .parse::<u64>()
                .map_err(|e| format!("{key}={v:?}: {e}"))
        })
        .transpose()
}

fn env_usize(env: &dyn EnvSource, key: &str) -> Result<Option<usize>, String> {
    env.get(key)
        .map(|v| {
            v.trim()
                .parse::<usize>()
                .map_err(|e| format!("{key}={v:?}: {e}"))
        })
        .transpose()
}

/// Booleans in the environment: `1/true/yes/on` and `0/false/no/off`, case
/// insensitive. Any other value is an error.
fn env_bool(env: &dyn EnvSource, key: &str) -> Result<Option<bool>, String> {
    match env.get(key) {
        None => Ok(None),
        Some(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            other => Err(format!(
                "{key}={other:?} is not a boolean (use 1/0, true/false, yes/no, on/off)"
            )),
        },
    }
}

fn env_list(env: &dyn EnvSource, keys: &[&str]) -> Option<Vec<String>> {
    for key in keys {
        if let Some(raw) = env.get(key) {
            return Some(split_list(&raw));
        }
    }
    None
}

/// Comma-separated lists, with whitespace tolerated and empty entries
/// dropped, matching how postvec parses its own endpoint GUCs.
pub fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a possibly-relative path against the engine root.
///
/// Relative certificate paths resolve against the root, so a unit that sets
/// `POSTVEC_SERVER_ROOT=/srv/postvec` and drops certificates there is
/// independent of where the JSON lives.
fn against_root(root: &Path, value: impl AsRef<Path>) -> PathBuf {
    let value = value.as_ref();
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        root.join(value)
    }
}

/// Merge the four layers. Pure: everything that touches the filesystem or the
/// network happens in the caller.
pub fn resolve(
    flags: &ServeArgs,
    file: &FileConfig,
    env: &dyn EnvSource,
    root: PathBuf,
    config_path: Option<PathBuf>,
) -> Result<Settings, String> {
    // --- ports -----------------------------------------------------------
    let gossip_flag = match (flags.gossip, flags.remote) {
        (Some(g), Some(r)) if g != r => {
            return Err(format!(
                "--gossip {g} and --remote {r} disagree; --remote is a deprecated spelling of \
                 --gossip, so pass one of them"
            ))
        }
        (Some(g), _) => Some(g),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    };

    let http_port = flags
        .http
        .or(env_u16(env, "POSTVEC_SERVER_HTTP")?)
        .or(file.http_port)
        .unwrap_or(DEFAULT_HTTP_PORT);
    let grpc_port = flags
        .grpc
        .or(env_u16(env, "POSTVEC_SERVER_GRPC")?)
        .or(file.grpc_port)
        .unwrap_or(DEFAULT_GRPC_PORT);
    let gossip_port = gossip_flag
        .or(env_u16(env, "POSTVEC_SERVER_GOSSIP")?)
        .or(file.gossip_port)
        .unwrap_or(DEFAULT_GOSSIP_PORT);
    let admin_port = flags
        .admin
        .or(env_u16(env, "POSTVEC_SERVER_ADMIN")?)
        .or(file.admin_port)
        .unwrap_or(DEFAULT_ADMIN_PORT);

    for (name, port) in [
        ("--http", http_port),
        ("--grpc", grpc_port),
        ("--gossip", gossip_port),
        ("--admin", admin_port),
    ] {
        if port == 0 {
            return Err(format!("{name} must be a real port, not 0"));
        }
    }
    // The admin listener binds loopback while the others usually bind
    // 0.0.0.0, which covers loopback. A shared number is a bind failure at
    // boot, or an admin route answering on a published port.
    let mut seen: Vec<(&str, u16)> = Vec::new();
    for entry in [
        ("--http", http_port),
        ("--grpc", grpc_port),
        ("--gossip", gossip_port),
        ("--admin", admin_port),
    ] {
        if let Some((other, _)) = seen.iter().find(|(_, p)| *p == entry.1) {
            return Err(format!(
                "{} and {} are both {}; every listener needs its own port",
                other, entry.0, entry.1
            ));
        }
        seen.push(entry);
    }

    // --- bind ------------------------------------------------------------
    let bind_raw = flags
        .bind
        .clone()
        .or_else(|| env.get("POSTVEC_SERVER_BIND"))
        .or_else(|| file.bind_address.clone())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let bind: IpAddr = bind_raw
        .trim()
        .parse()
        .map_err(|e| format!("bind address {bind_raw:?} is not an IP address: {e}"))?;

    // --- cluster ---------------------------------------------------------
    let peers = flags
        .peers
        .clone()
        .or_else(|| env_list(env, &["POSTVEC_SERVER_PEERS", "POSTVEC_SERVER_CLUSTER"]))
        .or_else(|| file.peers.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();

    let group = flags
        .group
        .clone()
        .or_else(|| env.get("POSTVEC_SERVER_GROUP"))
        .or_else(|| file.group.clone())
        .unwrap_or_else(|| DEFAULT_GROUP.to_string())
        .trim()
        .to_string();
    if group.is_empty() {
        return Err(
            "--group must not be empty; it is the tag that keeps unrelated fleets \
                    from merging"
                .to_string(),
        );
    }

    let advertise_raw = flags
        .advertise
        .clone()
        .or_else(|| env.get("POSTVEC_SERVER_ADVERTISE"))
        .or_else(|| file.advertise.clone());
    let advertise = advertise_raw
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|raw| {
            raw.parse::<IpAddr>().map_err(|e| {
                format!(
                    "advertise address {raw:?} is not an IP address: {e}\n\
                     (it is the host part other nodes dial, so it has to be literal — use \
                     --frontend for a name)"
                )
            })
        })
        .transpose()?;

    let frontend = flags
        .frontend
        .clone()
        .or_else(|| env.get("POSTVEC_SERVER_FRONTEND"))
        .or_else(|| file.frontend.clone())
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty());
    if let Some(url) = &frontend {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!(
                "--frontend {url:?} must be an absolute http:// or https:// URL; no scheme is \
                 inferred"
            ));
        }
    }

    // --- TLS -------------------------------------------------------------
    let insecure = if flags.insecure {
        true
    } else {
        env_bool(env, "POSTVEC_SERVER_INSECURE")?
            .or(file.insecure)
            .unwrap_or(false)
    };
    let tls = if insecure {
        None
    } else {
        let file_ssl = file.ssl.clone().unwrap_or_default();
        let cert = flags
            .ssl_cert
            .clone()
            .or_else(|| env.get("POSTVEC_SERVER_SSL_CERT").map(PathBuf::from))
            .or_else(|| file_ssl.cert.map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("certs/server.crt"));
        let key = flags
            .ssl_cert_key
            .clone()
            .or_else(|| env.get("POSTVEC_SERVER_SSL_KEY").map(PathBuf::from))
            .or_else(|| file_ssl.key.map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("certs/server.key"));
        Some(TlsPaths {
            cert: against_root(&root, cert),
            key: against_root(&root, key),
        })
    };

    // --- engine ----------------------------------------------------------
    let models = flags
        .models
        .clone()
        .or_else(|| env_list(env, &["POSTVEC_SERVER_MODELS"]))
        .or_else(|| file.models.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .collect::<Vec<_>>();

    // The server default nests under --root: the root is this process's
    // configuration anchor, and each node is administered with
    // `postvec provider ... --path <root>`.
    let providers_path = flags
        .providers_path
        .clone()
        .or_else(|| env.get("POSTVEC_SERVER_PROVIDERS_PATH").map(PathBuf::from))
        .or_else(|| file.providers_path.clone().map(PathBuf::from))
        .map(|p| against_root(&root, p))
        .unwrap_or_else(|| root.join("providers.d"));

    let predict_timeout_ms = flags
        .predict_timeout_ms
        .or(env_u64(env, "POSTVEC_SERVER_PREDICT_TIMEOUT_MS")?)
        .or(file.predict_timeout_ms)
        .unwrap_or(DEFAULT_PREDICT_TIMEOUT_MS);
    if predict_timeout_ms < MIN_PREDICT_TIMEOUT_MS {
        return Err(format!(
            "--predict-timeout-ms {predict_timeout_ms} is below the {MIN_PREDICT_TIMEOUT_MS} ms \
             floor; no model answers that fast, so this is a typo"
        ));
    }

    let max_inflight = flags
        .max_inflight
        .or(env_usize(env, "POSTVEC_SERVER_MAX_INFLIGHT")?)
        .or(file.max_inflight)
        .unwrap_or_else(default_max_inflight);
    if max_inflight == 0 {
        return Err("--max-inflight must be at least 1".to_string());
    }

    let max_resident_models = flags
        .max_resident_models
        .or(env_usize(env, "POSTVEC_SERVER_MAX_RESIDENT_MODELS")?)
        .or(file.max_resident_models)
        .unwrap_or(DEFAULT_MAX_RESIDENT_MODELS);
    if max_resident_models == 0 {
        return Err("--max-resident-models must be at least 1".to_string());
    }
    if !models.is_empty() && models.len() > max_resident_models {
        return Err(format!(
            "--models names {} model(s), above the resident ceiling of {max_resident_models}; \
             raise --max-resident-models or shorten the list",
            models.len()
        ));
    }

    let drain_delay_ms = flags
        .drain_delay_ms
        .or(env_u64(env, "POSTVEC_SERVER_DRAIN_DELAY_MS")?)
        .or(file.drain_delay_ms)
        .unwrap_or(DEFAULT_DRAIN_DELAY_MS);

    let warmup = if flags.no_warmup {
        false
    } else {
        env_bool(env, "POSTVEC_SERVER_WARMUP")?
            .or(file.warmup)
            .unwrap_or(true)
    };
    let metrics = if flags.no_metrics {
        false
    } else {
        env_bool(env, "POSTVEC_SERVER_METRICS")?
            .or(file.metrics)
            .unwrap_or(true)
    };

    let manage = if flags.manage {
        true
    } else {
        env_bool(env, "POSTVEC_SERVER_MANAGE")?
            .or(file.manage)
            .unwrap_or(false)
    };

    let log_level = flags
        .log_level
        .clone()
        .or_else(|| file.log_level.clone())
        .unwrap_or_else(|| "info".to_string());

    let web_ui = flags
        .web_ui
        .clone()
        .or_else(|| env.get("POSTVEC_SERVER_WEB_UI").map(PathBuf::from))
        .or_else(|| file.web_ui.clone().map(PathBuf::from))
        .map(|p| against_root(&root, p));

    let mut managed = file.managed.clone().unwrap_or_default();
    if let Some(dsn) = flags.sync.as_ref().or(flags.proxy_upstream.as_ref()) {
        managed = vec![crate::managed::ManagedDb {
            name: "default".into(),
            dsn: dsn.clone(),
            sync: flags.sync.is_some(),
            proxy_port: flags.proxy,
            poll_only: flags.poll_only,
            ..Default::default()
        }];
    } else if flags.proxy.is_some() {
        return Err("--proxy needs --sync or --proxy-upstream to name the database".into());
    }
    let mut names = std::collections::HashSet::new();
    let mut targets: Vec<(String, (String, u16, String))> = Vec::new();
    for db in &mut managed {
        db.validate()?;
        if !names.insert(db.name.clone()) {
            return Err("managed database names must be unique".into());
        }
        let o: sqlx::postgres::PgConnectOptions = db
            .dsn
            .parse()
            .map_err(|_| "invalid managed PostgreSQL DSN")?;
        let target = (
            o.get_host().to_string(),
            o.get_port(),
            o.get_database().unwrap_or(o.get_username()).to_string(),
        );
        if let Some((other, _)) = targets.iter().find(|(_, t)| *t == target) {
            return Err(format!(
                "managed databases {other} and {} point at the same database; use one entry",
                db.name
            ));
        }
        targets.push((db.name.clone(), target));
        db.password_file = db.password_file.as_ref().map(|p| against_root(&root, p));
        if let Some(port) = db.proxy_port {
            if let Some((other, _)) = seen.iter().find(|(_, p)| *p == port) {
                return Err(format!(
                    "{other} and the proxy for managed database {} are both {port}; every listener needs its own port",
                    db.name
                ));
            }
            seen.push(("proxy_port", port));
        }
    }
    Ok(Settings {
        managed,
        root,
        config_path,
        bind,
        http_port,
        grpc_port,
        gossip_port,
        admin_port,
        peers,
        group,
        advertise,
        frontend,
        tls,
        models,
        providers_path,
        predict_timeout: Duration::from_millis(predict_timeout_ms),
        max_inflight,
        max_resident_models,
        drain_delay: Duration::from_millis(drain_delay_ms),
        warmup,
        metrics,
        log_level,
        manage,
        web_ui,
    })
}

/// Read, parse and merge everything. The only impure step is reading the
/// file the search order settled on.
pub fn load(flags: &ServeArgs, env: &dyn EnvSource) -> Result<Settings, String> {
    let root = resolve_root(flags.root.as_deref(), env)?;
    let config_path = resolve_config_path(flags.config.as_deref(), env, &root)?;
    let file = match &config_path {
        Some(path) => {
            let raw = std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            FileConfig::parse(&raw, path)?
        }
        None => FileConfig::default(),
    };
    resolve(flags, &file, env, root, config_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn resolve_with(
        flags: ServeArgs,
        file: FileConfig,
        vars: &[(&str, &str)],
    ) -> Result<Settings, String> {
        resolve(&flags, &file, &env(vars), PathBuf::from("/srv/root"), None)
    }

    #[test]
    fn all_defaults_produce_a_runnable_single_node() {
        let s = resolve_with(ServeArgs::default(), FileConfig::default(), &[]).unwrap();
        assert_eq!(s.http_port, DEFAULT_HTTP_PORT);
        assert_eq!(s.grpc_port, DEFAULT_GRPC_PORT);
        assert_eq!(s.gossip_port, DEFAULT_GOSSIP_PORT);
        assert_eq!(s.admin_port, DEFAULT_ADMIN_PORT);
        assert_eq!(s.group, DEFAULT_GROUP);
        assert!(s.peers.is_empty(), "no peers means a single-node cluster");
        assert!(s.models.is_empty(), "empty means every enabled descriptor");
        assert!(s.tls.is_some(), "TLS is on unless --insecure");
        assert!(s.warmup);
        assert!(s.metrics);
        assert!(!s.manage, "public model mutation is opt-in");
        assert_eq!(
            s.drain_delay,
            Duration::from_millis(DEFAULT_DRAIN_DELAY_MS),
            "a signal must not close the socket before /ready has said 503"
        );
        assert_eq!(s.bind, "0.0.0.0".parse::<IpAddr>().unwrap());
        assert_eq!(
            s.providers_path,
            PathBuf::from("/srv/root/providers.d"),
            "providers.d nests under the root by default"
        );
    }

    #[test]
    fn providers_path_follows_the_precedence_and_the_root() {
        // Relative values anchor to the root, like the TLS paths.
        let s = resolve_with(
            ServeArgs {
                providers_path: Some(PathBuf::from("conf/providers.d")),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap();
        assert_eq!(
            s.providers_path,
            PathBuf::from("/srv/root/conf/providers.d")
        );

        // Absolute flag wins over env and file, and stays absolute.
        let file = FileConfig {
            providers_path: Some("/from/file".into()),
            ..Default::default()
        };
        let s = resolve_with(
            ServeArgs::default(),
            file.clone(),
            &[("POSTVEC_SERVER_PROVIDERS_PATH", "/from/env")],
        )
        .unwrap();
        assert_eq!(s.providers_path, PathBuf::from("/from/env"));
        let s = resolve_with(
            ServeArgs {
                providers_path: Some(PathBuf::from("/from/flag")),
                ..Default::default()
            },
            file,
            &[("POSTVEC_SERVER_PROVIDERS_PATH", "/from/env")],
        )
        .unwrap();
        assert_eq!(s.providers_path, PathBuf::from("/from/flag"));
    }

    /// The documented precedence, one field at a time.
    #[test]
    fn flag_beats_env_beats_file_beats_default() {
        let file = FileConfig {
            http_port: Some(1111),
            ..Default::default()
        };
        // default
        let s = resolve_with(ServeArgs::default(), FileConfig::default(), &[]).unwrap();
        assert_eq!(s.http_port, DEFAULT_HTTP_PORT);
        // file
        let s = resolve_with(ServeArgs::default(), file.clone(), &[]).unwrap();
        assert_eq!(s.http_port, 1111);
        // env over file
        let s = resolve_with(
            ServeArgs::default(),
            file.clone(),
            &[("POSTVEC_SERVER_HTTP", "2222")],
        )
        .unwrap();
        assert_eq!(s.http_port, 2222);
        // flag over env
        let s = resolve_with(
            ServeArgs {
                http: Some(3333),
                ..Default::default()
            },
            file,
            &[("POSTVEC_SERVER_HTTP", "2222")],
        )
        .unwrap();
        assert_eq!(s.http_port, 3333);
    }

    #[test]
    fn web_ui_follows_the_precedence_and_the_root() {
        let s = resolve_with(
            ServeArgs {
                web_ui: Some(PathBuf::from("server/ui")),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap();
        assert_eq!(s.web_ui.as_deref(), Some(Path::new("/srv/root/server/ui")));

        let file = FileConfig {
            web_ui: Some("/from/file".into()),
            ..Default::default()
        };
        let s = resolve_with(
            ServeArgs::default(),
            file.clone(),
            &[("POSTVEC_SERVER_WEB_UI", "/from/env")],
        )
        .unwrap();
        assert_eq!(s.web_ui.as_deref(), Some(Path::new("/from/env")));
        let s = resolve_with(
            ServeArgs {
                web_ui: Some(PathBuf::from("/from/flag")),
                ..Default::default()
            },
            file,
            &[("POSTVEC_SERVER_WEB_UI", "/from/env")],
        )
        .unwrap();
        assert_eq!(s.web_ui.as_deref(), Some(Path::new("/from/flag")));
    }

    /// An unrelated flag leaves a file-supplied value in place.
    #[test]
    fn an_absent_flag_does_not_clear_a_file_value() {
        let file = FileConfig {
            peers: Some(vec!["10.0.0.1".into(), "10.0.0.2".into()]),
            group: Some("fleet-a".into()),
            ..Default::default()
        };
        let s = resolve_with(
            ServeArgs {
                grpc: Some(4000),
                ..Default::default()
            },
            file,
            &[],
        )
        .unwrap();
        assert_eq!(s.grpc_port, 4000);
        assert_eq!(s.peers, ["10.0.0.1", "10.0.0.2"]);
        assert_eq!(s.group, "fleet-a");
    }

    #[test]
    fn unknown_file_keys_are_fatal() {
        let err = FileConfig::parse(r#"{"hub": {}}"#, Path::new("cfg.json")).unwrap_err();
        assert!(err.contains("hub"), "{err}");
        let err = FileConfig::parse(r#"{"htp_port": 1}"#, Path::new("cfg.json")).unwrap_err();
        assert!(err.contains("htp_port"), "{err}");
    }

    #[test]
    fn the_documented_example_file_parses() {
        let raw = r#"{
          "bind_address": "0.0.0.0",
          "http_port": 22222,
          "grpc_port": 33333,
          "gossip_port": 11111,
          "admin_port": 22223,
          "group": "postvec",
          "peers": ["10.0.0.10", "10.0.0.11"],
          "advertise": "10.0.0.10",
          "frontend": null,
          "ssl": { "cert": "certs/server.crt", "key": "certs/server.key" },
          "models": [],
          "predict_timeout_ms": 30000,
          "max_inflight": 8
        }"#;
        let file = FileConfig::parse(raw, Path::new("cfg.json")).unwrap();
        let s = resolve_with(ServeArgs::default(), file, &[]).unwrap();
        assert_eq!(s.peers, ["10.0.0.10", "10.0.0.11"]);
        assert_eq!(s.max_inflight, 8);
        assert!(s.frontend.is_none(), "explicit null means unset");
        assert_eq!(
            s.tls.unwrap().cert,
            PathBuf::from("/srv/root/certs/server.crt"),
            "relative cert paths resolve against the root, not the file"
        );
    }

    /// `//`-prefixed keys are comments.
    #[test]
    fn double_slash_keys_are_comments() {
        let file = FileConfig::parse(
            r#"{
                "//": "top-level note",
                "//group": "why this value",
                "group": "fleet-a",
                "ssl": { "//cert": "note", "cert": "a.crt" }
            }"#,
            Path::new("cfg.json"),
        )
        .unwrap();
        assert_eq!(file.group.as_deref(), Some("fleet-a"));
        assert_eq!(file.ssl.unwrap().cert.as_deref(), Some("a.crt"));
    }

    /// The example file ships next to the binary and must parse.
    #[test]
    fn the_shipped_example_file_parses_and_resolves() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("postvec-server.example.json");
        let raw =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let file = FileConfig::parse(&raw, &path).expect("the example must parse");
        let settings = resolve_with(ServeArgs::default(), file, &[]).expect("and resolve");
        assert_eq!(settings.group, "postvec");
        assert_eq!(settings.peers.len(), 3);
        assert!(settings.tls.is_some());
    }

    #[test]
    fn cluster_is_accepted_as_a_file_alias_for_peers() {
        let file =
            FileConfig::parse(r#"{"cluster": ["10.0.0.1"]}"#, Path::new("cfg.json")).unwrap();
        assert_eq!(
            file.peers.as_deref(),
            Some(["10.0.0.1".to_string()].as_slice())
        );
    }

    #[test]
    fn insecure_disables_tls_from_any_layer() {
        for (flags, vars, file) in [
            (
                ServeArgs {
                    insecure: true,
                    ..Default::default()
                },
                vec![],
                FileConfig::default(),
            ),
            (
                ServeArgs::default(),
                vec![("POSTVEC_SERVER_INSECURE", "1")],
                FileConfig::default(),
            ),
            (
                ServeArgs::default(),
                vec![],
                FileConfig {
                    insecure: Some(true),
                    ..Default::default()
                },
            ),
        ] {
            let s = resolve_with(flags, file, &vars).unwrap();
            assert!(s.tls.is_none());
        }
    }

    /// A flag cannot be "un-set", so `--insecure` beats a file that says
    /// false — but a file that says true is *not* overridden by the absence
    /// of the flag.
    #[test]
    fn insecure_flag_wins_over_a_false_file_value() {
        let s = resolve_with(
            ServeArgs {
                insecure: true,
                ..Default::default()
            },
            FileConfig {
                insecure: Some(false),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        assert!(s.tls.is_none());
    }

    #[test]
    fn absolute_cert_paths_are_left_alone() {
        let s = resolve_with(
            ServeArgs {
                ssl_cert: Some(PathBuf::from("/etc/tls/a.crt")),
                ssl_cert_key: Some(PathBuf::from("/etc/tls/a.key")),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap();
        let tls = s.tls.unwrap();
        assert_eq!(tls.cert, PathBuf::from("/etc/tls/a.crt"));
        assert_eq!(tls.key, PathBuf::from("/etc/tls/a.key"));
    }

    #[test]
    fn gossip_and_remote_must_agree() {
        let err = resolve_with(
            ServeArgs {
                gossip: Some(1),
                remote: Some(2),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("disagree"), "{err}");

        // Agreeing, or only one of them, is fine.
        for args in [
            ServeArgs {
                gossip: Some(9),
                remote: Some(9),
                ..Default::default()
            },
            ServeArgs {
                remote: Some(9),
                ..Default::default()
            },
        ] {
            let s = resolve_with(args, FileConfig::default(), &[]).unwrap();
            assert_eq!(s.gossip_port, 9);
        }
    }

    #[test]
    fn listeners_may_not_share_a_port() {
        let err = resolve_with(
            ServeArgs {
                admin: Some(DEFAULT_HTTP_PORT),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("own port"), "{err}");
    }

    #[test]
    fn a_bad_env_value_is_a_precise_error_not_a_silent_default() {
        let err = resolve_with(
            ServeArgs::default(),
            FileConfig::default(),
            &[("POSTVEC_SERVER_HTTP", "not-a-port")],
        )
        .unwrap_err();
        assert!(err.contains("POSTVEC_SERVER_HTTP"), "{err}");

        let err = resolve_with(
            ServeArgs::default(),
            FileConfig::default(),
            &[("POSTVEC_SERVER_INSECURE", "maybe")],
        )
        .unwrap_err();
        assert!(err.contains("not a boolean"), "{err}");
    }

    #[test]
    fn frontend_must_be_an_absolute_url() {
        let err = resolve_with(
            ServeArgs {
                frontend: Some("node-1:22222".into()),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn advertise_must_be_an_ip() {
        let err = resolve_with(
            ServeArgs {
                advertise: Some("node-1.internal".into()),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("not an IP address"), "{err}");
    }

    #[test]
    fn peer_lists_come_from_env_under_either_name() {
        for key in ["POSTVEC_SERVER_PEERS", "POSTVEC_SERVER_CLUSTER"] {
            let s = resolve_with(
                ServeArgs::default(),
                FileConfig::default(),
                &[(key, " 10.0.0.1 , 10.0.0.2 ,")],
            )
            .unwrap();
            assert_eq!(s.peers, ["10.0.0.1", "10.0.0.2"], "via {key}");
        }
    }

    #[test]
    fn timeouts_and_ceilings_are_sanity_checked() {
        let err = resolve_with(
            ServeArgs {
                predict_timeout_ms: Some(1),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("floor"), "{err}");

        let err = resolve_with(
            ServeArgs {
                max_inflight: Some(0),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("at least 1"), "{err}");

        let err = resolve_with(
            ServeArgs {
                models: Some(vec!["a".into(), "b".into(), "c".into()]),
                max_resident_models: Some(2),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap_err();
        assert!(err.contains("resident ceiling"), "{err}");
    }

    #[test]
    fn proxy_flags_build_a_managed_entry() {
        let s = resolve_with(
            ServeArgs {
                proxy_upstream: Some("postgresql://u@db/app".into()),
                proxy: Some(5433),
                ..Default::default()
            },
            FileConfig::default(),
            &[],
        )
        .unwrap();
        assert_eq!(s.managed.len(), 1);
        assert!(!s.managed[0].sync);
        assert_eq!(s.managed[0].proxy_port, Some(5433));
        let twice = FileConfig {
            managed: Some(
                ["a", "b"]
                    .map(|name| crate::managed::ManagedDb {
                        name: name.into(),
                        dsn: "postgresql://u@db/app".into(),
                        ..Default::default()
                    })
                    .to_vec(),
            ),
            ..Default::default()
        };
        assert!(resolve_with(ServeArgs::default(), twice, &[])
            .unwrap_err()
            .contains("same database"));
        for flags in [
            ServeArgs {
                proxy: Some(5433),
                ..Default::default()
            },
            ServeArgs {
                sync: Some("postgresql://u@db/app".into()),
                proxy: Some(DEFAULT_HTTP_PORT),
                ..Default::default()
            },
        ] {
            assert!(resolve_with(flags, FileConfig::default(), &[]).is_err());
        }
    }

    #[test]
    fn autodetected_inflight_stays_inside_its_band() {
        let n = default_max_inflight();
        assert!((MIN_AUTO_INFLIGHT..=MAX_AUTO_INFLIGHT).contains(&n), "{n}");
    }

    #[test]
    fn root_resolution_order() {
        let flag = PathBuf::from("/from/flag");
        assert_eq!(
            resolve_root(Some(&flag), &env(&[("POSTVEC_SERVER_ROOT", "/from/env")])).unwrap(),
            flag
        );
        assert_eq!(
            resolve_root(None, &env(&[("POSTVEC_SERVER_ROOT", "/from/env")])).unwrap(),
            PathBuf::from("/from/env")
        );
        assert_eq!(
            resolve_root(None, &env(&[])).unwrap(),
            PathBuf::from(DEFAULT_ROOT)
        );
    }

    #[test]
    fn an_explicitly_named_missing_config_file_is_an_error() {
        let missing = PathBuf::from("/nonexistent/postvec-server.json");
        let err = resolve_config_path(Some(&missing), &env(&[]), Path::new("/srv")).unwrap_err();
        assert!(err.contains("requested explicitly"), "{err}");
    }

    #[test]
    fn a_missing_implicit_config_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_config_path(None, &env(&[]), dir.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_root_config_file_is_found_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CONFIG_FILENAME);
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(
            resolve_config_path(None, &env(&[]), dir.path()).unwrap(),
            Some(path)
        );
    }

    #[test]
    fn load_reads_the_file_the_search_order_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILENAME),
            r#"{"group": "from-file", "insecure": true}"#,
        )
        .unwrap();
        let flags = ServeArgs {
            root: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let s = load(&flags, &env(&[])).unwrap();
        assert_eq!(s.group, "from-file");
        assert!(s.tls.is_none());
        assert_eq!(
            s.config_path.as_deref(),
            Some(dir.path().join(CONFIG_FILENAME).as_path())
        );
    }
}
