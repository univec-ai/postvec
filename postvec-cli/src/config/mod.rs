//! The configuration snippet the CLI owns, and the state file that records
//! that ownership.
//!
//! The CLI writes exactly one file (`conf.d/99-postvec.conf`) and never edits
//! `postgresql.conf`. That single-file ownership is what makes uninstall
//! honest: removing the file restores whatever the operator had before.

pub mod guc;
pub mod owned;

use crate::cli::Mode;
use crate::error::Result;
use crate::validate::{self, GrpcEndpoint, HttpEndpoint};
use std::net::SocketAddr;
use std::path::PathBuf;

/// Marker on line 1 of the owned file. Its presence identifies the file as
/// CLI-managed even when the state file has been lost.
pub const MANAGED_MARKER: &str = "# Managed by postvec.";

/// Basename of the owned snippet. The `99-` prefix keeps it last among
/// `conf.d` files sorted by name, so it wins over other snippets.
pub const OWNED_FILE_NAME: &str = "99-postvec.conf";

/// Every setting the CLI may render. Doctor also queries exactly this set
/// (plus a few read-only ones) so the two views cannot drift.
pub const MANAGED_SETTINGS: &[&str] = &[
    "shared_preload_libraries",
    "postvec.database",
    "postvec.mode",
    "postvec.ninference_grpc_endpoints",
    "postvec.ninference_http_endpoints",
    "postvec.ninference_path",
    "postvec.embedded_models",
    "postvec.embedded_listen",
    "postvec.embedded_http_listen",
    "postvec.providers_path",
];

/// Defaults the extension applies when the listener GUCs are unset
/// (`postvec/src/gucs.rs`).
pub const DEFAULT_EMBEDDED_LISTEN: &str = "127.0.0.1:33433";
pub const DEFAULT_EMBEDDED_HTTP_LISTEN: &str = "127.0.0.1:33434";

/// Where the packages install the engine root, and what
/// `postvec.ninference_path` defaults to
/// (`postvec/src/gucs.rs::DEFAULT_NINFERENCE_PATH`).
///
/// Duplicated rather than shared because the extension is a pgrx crate this
/// one cannot link. The pair is asserted in `cli.rs`; change both together.
pub const DEFAULT_ENGINE_ROOT: &str = "/opt/postvec/ninference";

/// What `postvec.providers_path` defaults to
/// (`postvec/src/gucs.rs::DEFAULT_PROVIDERS_PATH`). Deliberately outside the
/// engine root: the model tree gets rsynced and baked into images; the
/// credential directory must not ride along.
pub const DEFAULT_PROVIDERS_PATH: &str = "/etc/postvec/providers.d";

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RemoteSettings {
    pub grpc: Vec<GrpcEndpoint>,
    pub http: Vec<HttpEndpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EmbeddedSettings {
    pub path: PathBuf,
    /// providers.d override. `None` (the default) is not rendered at all, so
    /// the extension's own default stays in effect — unlike
    /// `embedded_models`, an omitted value here means exactly the default
    /// path and nothing lower-precedence can disagree with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers_path: Option<PathBuf>,
    /// Empty means "scan-load every enabled model under <root>/models".
    pub models: Vec<String>,
    pub grpc_listen: SocketAddr,
    pub http_listen: SocketAddr,
}

impl EmbeddedSettings {
    pub fn new(
        path: PathBuf,
        providers_path: Option<PathBuf>,
        models: Vec<String>,
        grpc_listen: Option<SocketAddr>,
        http_listen: Option<SocketAddr>,
    ) -> Self {
        Self {
            path,
            providers_path,
            models,
            grpc_listen: grpc_listen
                .unwrap_or_else(|| DEFAULT_EMBEDDED_LISTEN.parse().expect("valid default")),
            http_listen: http_listen
                .unwrap_or_else(|| DEFAULT_EMBEDDED_HTTP_LISTEN.parse().expect("valid default")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum InferenceSettings {
    Grpc(RemoteSettings),
    Embedded(EmbeddedSettings),
}

impl InferenceSettings {
    pub fn mode(&self) -> Mode {
        match self {
            InferenceSettings::Grpc(_) => Mode::Grpc,
            InferenceSettings::Embedded(_) => Mode::Embedded,
        }
    }
}

/// The complete desired content of the owned snippet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct DesiredConfig {
    /// The merged preload list to render, or `None` when postvec is already
    /// preloaded by configuration the CLI does not own. Restating the setting
    /// in that case would silently take ownership of the operator's value and
    /// make uninstall unable to keep its promise.
    pub preload: Option<Vec<String>>,
    /// Sorted and deduplicated: rendered output must be byte-stable so a
    /// rerun is provably a no-op.
    pub databases: Vec<String>,
    pub inference: InferenceSettings,
}

impl DesiredConfig {
    pub fn mode(&self) -> Mode {
        self.inference.mode()
    }

    /// Render the file. Deterministic for a given input.
    pub fn render(&self, cli_version: &str) -> Result<String> {
        let mut out = String::new();
        out.push_str(MANAGED_MARKER);
        out.push_str(" Manual edits are refused; use `postvec setup`.\n");
        out.push_str(&format!("# postvec-cli {cli_version}\n"));

        if let Some(preload) = &self.preload {
            let rendered = guc::render_library_list(preload);
            out.push_str(&format!(
                "shared_preload_libraries = {}\n",
                validate::config_literal(&rendered)?
            ));
        }
        out.push_str(&format!(
            "postvec.database = {}\n",
            validate::config_literal(&guc::render_extension_list(&self.databases))?
        ));
        out.push_str(&format!(
            "postvec.mode = {}\n",
            validate::config_literal(self.mode().as_guc())?
        ));
        match &self.inference {
            InferenceSettings::Grpc(remote) => {
                let grpc: Vec<String> = remote.grpc.iter().map(|e| e.authority.clone()).collect();
                let http: Vec<String> = remote.http.iter().map(|e| e.base.clone()).collect();
                out.push_str(&format!(
                    "postvec.ninference_grpc_endpoints = {}\n",
                    validate::config_literal(&guc::render_extension_list(&grpc))?
                ));
                out.push_str(&format!(
                    "postvec.ninference_http_endpoints = {}\n",
                    validate::config_literal(&guc::render_extension_list(&http))?
                ));
            }
            InferenceSettings::Embedded(embedded) => {
                out.push_str(&format!(
                    "postvec.ninference_path = {}\n",
                    validate::config_literal(&embedded.path.display().to_string())?
                ));
                // Always rendered, including empty. An omitted setting cannot
                // override a lower-precedence source, and — because activation
                // compares only the settings the plan asserts — omitting it
                // would let a previous explicit model list stay in effect with
                // no restart planned. An empty value is exactly what the
                // extension reads as "scan-load everything enabled".
                out.push_str(&format!(
                    "postvec.embedded_models = {}{}\n",
                    validate::config_literal(&guc::render_extension_list(&embedded.models))?,
                    if embedded.models.is_empty() {
                        "   # empty: load every enabled model under <root>/models"
                    } else {
                        ""
                    }
                ));
                out.push_str(&format!(
                    "postvec.embedded_listen = {}\n",
                    validate::config_literal(&embedded.grpc_listen.to_string())?
                ));
                out.push_str(&format!(
                    "postvec.embedded_http_listen = {}\n",
                    validate::config_literal(&embedded.http_listen.to_string())?
                ));
                // Rendered only when the operator overrode it: unset means
                // the extension's own default path, and restating a default
                // would take ownership of a value setup was never given.
                if let Some(providers_path) = &embedded.providers_path {
                    out.push_str(&format!(
                        "postvec.providers_path = {}\n",
                        validate::config_literal(&providers_path.display().to_string())?
                    ));
                }
            }
        }
        Ok(out)
    }

    /// The settings this configuration asserts, as (name, expected effective
    /// value) pairs. Used both for the offline `postgres -C` validation and
    /// for the post-restart `pg_settings` proof.
    pub fn expected_settings(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if let Some(preload) = &self.preload {
            // The server reports the value as written, so compare against the
            // rendered list rather than the parsed items.
            out.push((
                "shared_preload_libraries",
                guc::render_library_list(preload),
            ));
        }
        out.push((
            "postvec.database",
            guc::render_extension_list(&self.databases),
        ));
        out.push(("postvec.mode", self.mode().as_guc().to_string()));
        match &self.inference {
            InferenceSettings::Grpc(remote) => {
                out.push((
                    "postvec.ninference_grpc_endpoints",
                    remote
                        .grpc
                        .iter()
                        .map(|e| e.authority.clone())
                        .collect::<Vec<_>>()
                        .join(","),
                ));
                out.push((
                    "postvec.ninference_http_endpoints",
                    remote
                        .http
                        .iter()
                        .map(|e| e.base.clone())
                        .collect::<Vec<_>>()
                        .join(","),
                ));
            }
            InferenceSettings::Embedded(embedded) => {
                out.push((
                    "postvec.ninference_path",
                    embedded.path.display().to_string(),
                ));
                out.push((
                    "postvec.embedded_models",
                    guc::render_extension_list(&embedded.models),
                ));
                out.push(("postvec.embedded_listen", embedded.grpc_listen.to_string()));
                out.push((
                    "postvec.embedded_http_listen",
                    embedded.http_listen.to_string(),
                ));
                if let Some(providers_path) = &embedded.providers_path {
                    out.push((
                        "postvec.providers_path",
                        providers_path.display().to_string(),
                    ));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote() -> DesiredConfig {
        DesiredConfig {
            preload: Some(vec!["pg_stat_statements".into(), "postvec".into()]),
            databases: vec!["analytics".into(), "univec".into()],
            inference: InferenceSettings::Grpc(RemoteSettings {
                grpc: validate::grpc_endpoints(&["192.0.2.2:33333".into()]).unwrap(),
                http: validate::http_endpoints(&["https://192.0.2.2:22222".into()]).unwrap(),
            }),
        }
    }

    fn embedded() -> DesiredConfig {
        DesiredConfig {
            preload: Some(vec!["pg_stat_statements".into(), "postvec".into()]),
            databases: vec!["univec".into()],
            inference: InferenceSettings::Embedded(EmbeddedSettings::new(
                PathBuf::from("/opt/ninference"),
                None,
                vec!["baai-bge-m3".into(), "embed-bridge".into()],
                None,
                None,
            )),
        }
    }

    #[test]
    fn remote_rendering_is_stable_and_complete() {
        let expected = "\
# Managed by postvec. Manual edits are refused; use `postvec setup`.
# postvec-cli 0.1.0
shared_preload_libraries = 'pg_stat_statements,postvec'
postvec.database = 'analytics,univec'
postvec.mode = 'grpc'
postvec.ninference_grpc_endpoints = '192.0.2.2:33333'
postvec.ninference_http_endpoints = 'https://192.0.2.2:22222'
";
        assert_eq!(remote().render("0.1.0").unwrap(), expected);
        assert_eq!(remote().render("0.1.0").unwrap(), expected, "deterministic");
    }

    #[test]
    fn embedded_rendering_includes_listeners_and_models() {
        let rendered = embedded().render("0.1.0").unwrap();
        assert!(rendered.contains("postvec.mode = 'embedded'"));
        assert!(rendered.contains("postvec.ninference_path = '/opt/ninference'"));
        assert!(rendered.contains("postvec.embedded_models = 'baai-bge-m3,embed-bridge'"));
        assert!(rendered.contains("postvec.embedded_listen = '127.0.0.1:33433'"));
        assert!(rendered.contains("postvec.embedded_http_listen = '127.0.0.1:33434'"));
        assert!(!rendered.contains("ninference_grpc_endpoints"));
    }

    /// providers_path renders only when the operator overrode it: unset means
    /// the extension's own default, and restating a default would take
    /// ownership of a value setup was never given.
    #[test]
    fn providers_path_renders_only_when_overridden() {
        let rendered = embedded().render("0.1.0").unwrap();
        assert!(!rendered.contains("postvec.providers_path"));

        let mut cfg = embedded();
        if let InferenceSettings::Embedded(e) = &mut cfg.inference {
            e.providers_path = Some(PathBuf::from("/srv/providers.d"));
        }
        let rendered = cfg.render("0.1.0").unwrap();
        assert!(rendered.contains("postvec.providers_path = '/srv/providers.d'"));
        assert!(cfg
            .expected_settings()
            .iter()
            .any(|(name, value)| *name == "postvec.providers_path" && value == "/srv/providers.d"));
    }

    /// An empty list must be rendered, not omitted: an omitted setting cannot
    /// override whatever set it before, and a plan that does not assert it
    /// cannot notice that the old value is still in effect.
    #[test]
    fn scan_load_renders_an_explicit_empty_list() {
        let mut cfg = embedded();
        if let InferenceSettings::Embedded(e) = &mut cfg.inference {
            e.models.clear();
        }
        let rendered = cfg.render("0.1.0").unwrap();
        assert!(rendered.contains("postvec.embedded_models = ''"));
        assert!(
            rendered.contains("load every enabled model"),
            "the empty value's meaning is spelled out for whoever reads the file"
        );
        assert_eq!(
            cfg.expected_settings()
                .into_iter()
                .find(|(name, _)| *name == "postvec.embedded_models")
                .map(|(_, value)| value),
            Some(String::new()),
            "the plan must assert the empty value so a stale list forces a restart"
        );
    }

    #[test]
    fn foreign_preload_is_not_restated() {
        let mut cfg = remote();
        cfg.preload = None;
        let rendered = cfg.render("0.1.0").unwrap();
        assert!(!rendered.contains("shared_preload_libraries"));
        assert!(!cfg
            .expected_settings()
            .iter()
            .any(|(n, _)| *n == "shared_preload_libraries"));
    }

    #[test]
    fn expected_settings_match_rendered_values() {
        for cfg in [remote(), embedded()] {
            let rendered = cfg.render("0.1.0").unwrap();
            for (name, value) in cfg.expected_settings() {
                let line = format!("{name} = '{value}'");
                assert!(
                    rendered.contains(&line),
                    "expected setting {line:?} missing from:\n{rendered}"
                );
            }
        }
    }

    #[test]
    fn managed_settings_covers_everything_rendered() {
        for cfg in [remote(), embedded()] {
            for (name, _) in cfg.expected_settings() {
                assert!(
                    MANAGED_SETTINGS.contains(&name),
                    "{name} is rendered but not in MANAGED_SETTINGS, so doctor would not see it"
                );
            }
        }
    }
}
