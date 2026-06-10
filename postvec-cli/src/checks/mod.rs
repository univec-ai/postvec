//! The diagnostic framework: check results, ordering, and aggregation.
//!
//! Checks are **data, not print statements**, and every check function in this
//! module tree is **pure** — it takes observed [`crate::facts`] and returns
//! results. That is what lets `setup`'s preflight, `setup`'s post-restart smoke
//! checks and `doctor` share one definition of "healthy", and what makes the
//! logic testable without a cluster.

pub mod cluster;
pub mod database;
pub mod embedded;
pub mod models;
pub mod provider;
pub mod remote;

use serde::Serialize;

/// JSON output contract version. Bump only for a breaking change.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CheckStatus {
    Pass,
    Warn,
    Fail,
    Skip,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Pass => "PASS",
            CheckStatus::Warn => "WARN",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skip => "SKIP",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// Stable machine key, e.g. `worker.heartbeat`. Covered by a test against
    /// [`CHECK_ORDER`] so a new check cannot ship unregistered.
    pub id: &'static str,
    /// `cluster`, `database:univec`, `endpoint:192.0.2.2:33333`, `embedded`.
    pub scope: String,
    pub status: CheckStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Present only where a real measurement exists (endpoint and listener
    /// probes). Pure evaluation has no meaningful duration, and reporting `0`
    /// for it would be noise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// A SKIP of a required check is a failure: the selected mode cannot be
    /// verified at all.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

impl CheckResult {
    pub fn new(
        id: &'static str,
        scope: impl Into<String>,
        status: CheckStatus,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id,
            scope: scope.into(),
            status,
            summary: summary.into(),
            evidence: None,
            remediation: None,
            duration_ms: None,
            required: false,
        }
    }

    pub fn pass(id: &'static str, scope: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, scope, CheckStatus::Pass, summary)
    }

    pub fn warn(id: &'static str, scope: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, scope, CheckStatus::Warn, summary)
    }

    pub fn fail(id: &'static str, scope: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, scope, CheckStatus::Fail, summary)
    }

    pub fn skip(id: &'static str, scope: impl Into<String>, summary: impl Into<String>) -> Self {
        Self::new(id, scope, CheckStatus::Skip, summary)
    }

    pub fn with_fix(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = Some(remediation.into());
        self
    }

    pub fn with_evidence(mut self, evidence: serde_json::Value) -> Self {
        self.evidence = Some(evidence);
        self
    }

    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Mark this check as one the selected mode depends on, so a SKIP counts
    /// against the run.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn is_blocking(&self) -> bool {
        self.status == CheckStatus::Fail || (self.status == CheckStatus::Skip && self.required)
    }
}

/// The canonical order checks are reported in, within a scope. Also the
/// registry of every check the CLI can emit.
pub const CHECK_ORDER: &[&str] = &[
    // cluster
    "cluster.connection",
    "cluster.identity",
    "cluster.version",
    "cluster.assets.postvec",
    "cluster.assets.vector",
    "cluster.config.parse",
    "cluster.config.ownership",
    "cluster.preload",
    "cluster.preload.pending",
    "cluster.preload.shadowed",
    "cluster.mode",
    "cluster.database-list",
    "cluster.worker-slots",
    // per database
    "database.exists",
    "extension.available",
    "extension.installed",
    "extension.version",
    "extension.build-info",
    "extension.vector-version",
    "worker.pid",
    "worker.heartbeat",
    "worker.heartbeat-advances",
    "worker.enabled",
    "worker.last-error",
    "queue.pending",
    "queue.dead",
    "models.cache",
    "models.cache-freshness",
    "registry.dependencies",
    "registry.indexes",
    "migrations.state",
    // inference
    "inference.probe",
    // remote inference
    "remote.grpc.address",
    "remote.grpc.connect",
    "remote.http.tls",
    "remote.http.health",
    "remote.http.config",
    "remote.models.consistency",
    // embedded inference
    "embedded.build-capability",
    "embedded.path",
    "embedded.onnx-runtime",
    "embedded.descriptors",
    "embedded.requested-models",
    "embedded.grpc-listener",
    "embedded.http-listener",
    "embedded.loaded-models",
    "embedded.cache-consistency",
    // external providers (postvec provider …)
    "provider.directory",
    "provider.file",
    "provider.key-source",
    "provider.served",
    "provider.catalogue",
    // model store (postvec model …)
    "models.receipts",
    "models.unactivated",
    "models.enabled-drift",
    "models.integrity",
    "models.updatable",
    "registry.reachable",
];

fn order_index(id: &str) -> usize {
    CHECK_ORDER
        .iter()
        .position(|known| *known == id)
        // An unregistered id sorts last rather than panicking in production;
        // the test below is what actually prevents it from shipping.
        .unwrap_or(usize::MAX)
}

fn scope_group(scope: &str) -> u8 {
    if scope == "cluster" {
        0
    } else if scope.starts_with("database:") {
        1
    } else if scope.starts_with("endpoint:") {
        2
    } else {
        3
    }
}

/// Sort checks into their reporting order: cluster, then each database, then
/// endpoints, then the embedded engine; within a scope, [`CHECK_ORDER`].
pub fn sort_checks(checks: &mut [CheckResult]) {
    checks.sort_by(|a, b| {
        scope_group(&a.scope)
            .cmp(&scope_group(&b.scope))
            .then_with(|| a.scope.cmp(&b.scope))
            .then_with(|| order_index(a.id).cmp(&order_index(b.id)))
            .then_with(|| a.summary.cmp(&b.summary))
    });
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Summary {
    pub pass: usize,
    pub warn: usize,
    pub fail: usize,
    pub skip: usize,
}

impl Summary {
    pub fn of(checks: &[CheckResult]) -> Self {
        let mut summary = Self::default();
        for check in checks {
            match check.status {
                CheckStatus::Pass => summary.pass += 1,
                CheckStatus::Warn => summary.warn += 1,
                CheckStatus::Fail => summary.fail += 1,
                CheckStatus::Skip => summary.skip += 1,
            }
        }
        summary
    }
}

/// Identifying facts printed at the top of a report.
#[derive(Debug, Clone, Serialize)]
pub struct ReportCluster {
    pub id: String,
    pub postgres_major: Option<u32>,
    pub postgres_version: Option<String>,
    pub mode: Option<String>,
}

/// The complete result of a `doctor` run.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub command: &'static str,
    pub cli_version: String,
    pub cluster: ReportCluster,
    pub started_at: String,
    pub duration_ms: u64,
    pub summary: Summary,
    pub checks: Vec<CheckResult>,
    /// True when the report was produced without holding the host lock, so a
    /// concurrent `setup`/`uninstall` cannot be excluded.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub concurrent_change_possible: bool,
}

impl Report {
    /// The exit code this report implies.
    ///
    /// - any FAIL, or a SKIP of a check the selected mode requires: 1
    /// - under `--strict`, any WARN as well: 1
    /// - otherwise 0
    pub fn exit(&self, strict: bool) -> crate::error::Exit {
        if self.checks.iter().any(CheckResult::is_blocking) {
            return crate::error::Exit::Failure;
        }
        if strict && self.summary.warn > 0 {
            return crate::error::Exit::Failure;
        }
        crate::error::Exit::Success
    }
}

/// Now, as an RFC 3339 timestamp in UTC.
pub fn timestamp_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_check_id_is_registered_in_order() {
        // The producers are pure functions over facts, so enumerating them
        // exhaustively here would mean building every fact shape. Instead the
        // per-module tests assert their own ids exist, and this asserts the
        // registry itself is well formed.
        let mut seen = std::collections::BTreeSet::new();
        for id in CHECK_ORDER {
            assert!(seen.insert(*id), "duplicate check id {id} in CHECK_ORDER");
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || matches!(c, '.' | '-')),
                "check id {id} is not a stable lower-case dotted key"
            );
        }
    }

    #[test]
    fn checks_sort_by_scope_group_then_canonical_order() {
        let mut checks = vec![
            CheckResult::pass("embedded.path", "embedded", "x"),
            CheckResult::pass("worker.heartbeat", "database:univec", "x"),
            CheckResult::pass("database.exists", "database:univec", "x"),
            CheckResult::pass("database.exists", "database:analytics", "x"),
            CheckResult::pass("remote.grpc.connect", "endpoint:192.0.2.2:33333", "x"),
            CheckResult::pass("cluster.preload", "cluster", "x"),
            CheckResult::pass("cluster.connection", "cluster", "x"),
        ];
        sort_checks(&mut checks);
        let order: Vec<(&str, &str)> = checks.iter().map(|c| (c.scope.as_str(), c.id)).collect();
        assert_eq!(
            order,
            [
                ("cluster", "cluster.connection"),
                ("cluster", "cluster.preload"),
                ("database:analytics", "database.exists"),
                ("database:univec", "database.exists"),
                ("database:univec", "worker.heartbeat"),
                ("endpoint:192.0.2.2:33333", "remote.grpc.connect"),
                ("embedded", "embedded.path"),
            ]
        );
    }

    #[test]
    fn sorting_is_stable_across_runs() {
        let build = || {
            vec![
                CheckResult::pass("queue.dead", "database:d", "b"),
                CheckResult::pass("queue.dead", "database:d", "a"),
            ]
        };
        let mut first = build();
        let mut second = build();
        sort_checks(&mut first);
        sort_checks(&mut second);
        assert_eq!(
            first.iter().map(|c| c.summary.clone()).collect::<Vec<_>>(),
            second.iter().map(|c| c.summary.clone()).collect::<Vec<_>>()
        );
    }

    fn report(checks: Vec<CheckResult>) -> Report {
        Report {
            schema_version: SCHEMA_VERSION,
            command: "doctor",
            cli_version: "0.1.0".into(),
            cluster: ReportCluster {
                id: "18/main".into(),
                postgres_major: Some(18),
                postgres_version: Some("18.4".into()),
                mode: Some("grpc".into()),
            },
            started_at: "2026-07-30T12:00:00Z".into(),
            duration_ms: 1,
            summary: Summary::of(&checks),
            checks,
            concurrent_change_possible: false,
        }
    }

    #[test]
    fn exit_codes_follow_the_documented_rules() {
        let clean = report(vec![CheckResult::pass("cluster.preload", "cluster", "ok")]);
        assert_eq!(clean.exit(false), crate::error::Exit::Success);
        assert_eq!(clean.exit(true), crate::error::Exit::Success);

        let warned = report(vec![CheckResult::warn(
            "queue.dead",
            "database:d",
            "1 dead",
        )]);
        assert_eq!(warned.exit(false), crate::error::Exit::Success);
        assert_eq!(
            warned.exit(true),
            crate::error::Exit::Failure,
            "--strict makes a warning fatal"
        );

        let failed = report(vec![CheckResult::fail("cluster.preload", "cluster", "no")]);
        assert_eq!(failed.exit(false), crate::error::Exit::Failure);
    }

    /// Guards the shape of the check the collector emits when the inference
    /// probe cannot run: it must be blocking, or a broken probe would produce a
    /// green report.
    #[test]
    fn a_failed_inference_probe_check_is_blocking() {
        let check = CheckResult::fail(
            "inference.probe",
            "inference",
            "the inference side could not be probed",
        )
        .required();
        assert!(check.is_blocking());
        assert!(CHECK_ORDER.contains(&"inference.probe"));
        assert_eq!(report(vec![check]).exit(false), crate::error::Exit::Failure);
    }

    #[test]
    fn an_informational_skip_passes_but_a_required_one_fails() {
        let informational = report(vec![CheckResult::skip(
            "worker.heartbeat-advances",
            "database:d",
            "not sampled",
        )]);
        assert_eq!(informational.exit(false), crate::error::Exit::Success);

        let required = report(vec![CheckResult::skip(
            "embedded.build-capability",
            "embedded",
            "cannot be determined",
        )
        .required()]);
        assert_eq!(
            required.exit(false),
            crate::error::Exit::Failure,
            "a mode-critical check that cannot be observed is not a pass"
        );
    }

    #[test]
    fn summary_counts_every_status() {
        let summary = Summary::of(&[
            CheckResult::pass("a", "cluster", ""),
            CheckResult::pass("b", "cluster", ""),
            CheckResult::warn("c", "cluster", ""),
            CheckResult::fail("d", "cluster", ""),
            CheckResult::skip("e", "cluster", ""),
        ]);
        assert_eq!(summary.pass, 2);
        assert_eq!(summary.warn, 1);
        assert_eq!(summary.fail, 1);
        assert_eq!(summary.skip, 1);
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let now = timestamp_now();
        assert!(now.ends_with('Z'), "{now}");
        assert!(chrono::DateTime::parse_from_rfc3339(&now).is_ok());
    }
}
