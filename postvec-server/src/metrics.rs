//! Prometheus text-format metrics from plain atomics.
//!
//! Error labels are a closed set: the `shared::ErrorCode` strings that also
//! travel as `x-ravenna-error-code`. An unknown code folds into `OTHER`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const METHOD_EMBED: usize = 0;
pub const METHOD_CONVERT: usize = 1;
const METHODS: [&str; 2] = ["embed_texts", "convert_embeddings"];

/// Every label `postvec_server_request_errors_total` can carry.
///
/// The first thirteen are `shared::ErrorCode` in declaration order — the
/// pinned wire taxonomy. The rest cover refusals raised by the transport
/// itself, which carry a gRPC status but no `x-ravenna-error-code`.
const CODES: [&str; 20] = [
    "INTERNAL_ERROR",
    "INVALID_INPUT",
    "TIMEOUT",
    "MODEL_NOT_FOUND",
    "MODEL_NOT_LOADED",
    "MODEL_DISABLED",
    "CONTEXT_LENGTH_EXCEEDED",
    "GPU_OUT_OF_MEMORY",
    "CPU_OVERLOAD",
    "BRIDGE_PATH_NOT_FOUND",
    "CONVERTER_NOT_FOUND",
    "TARGET_RESTRICTED",
    "UPSTREAM_SERVICE_UNAVAILABLE",
    // Transport-level refusals, by gRPC status name.
    "INVALID_ARGUMENT",
    "DEADLINE_EXCEEDED",
    "RESOURCE_EXHAUSTED",
    "UNAVAILABLE",
    "FAILED_PRECONDITION",
    "NOT_FOUND",
    "OTHER",
];
const CODE_OTHER: usize = CODES.len() - 1;

/// Upper bounds, in seconds. Spread for inference: a warm MiniLM batch is
/// single-digit milliseconds, a cold bridge chain is seconds, and the
/// default execution ceiling is 30 s.
const DURATION_BUCKETS: [f64; 14] = [
    0.005,
    0.01,
    0.025,
    0.05,
    0.1,
    0.25,
    0.5,
    1.0,
    2.5,
    5.0,
    10.0,
    30.0,
    60.0,
    f64::INFINITY,
];

#[derive(Debug, Default)]
struct MethodMetrics {
    requests: AtomicU64,
    errors: [AtomicU64; CODES.len()],
    items: AtomicU64,
    /// Cumulative bucket counts, in the Prometheus `le` sense (each bucket
    /// is rendered as the running total up to and including its bound).
    buckets: [AtomicU64; DURATION_BUCKETS.len()],
    /// Nanoseconds, summed. Rendered as seconds.
    duration_nanos: AtomicU64,
}

#[derive(Debug, Default)]
pub struct Metrics {
    methods: [MethodMetrics; METHODS.len()],
    in_flight: AtomicU64,
    admin_loads: AtomicU64,
    admin_unloads: AtomicU64,
    admin_refusals: AtomicU64,
    config_requests: AtomicU64,
    /// Boot-time warmup predictions that failed. A non-zero value here with
    /// a green `/ready` means a model is resident but has never successfully
    /// answered.
    warmup_failures: AtomicU64,
}

/// A snapshot of everything the renderer needs that does not live in the
/// counters: engine and cluster state, sampled at scrape time.
pub struct Snapshot {
    pub version: &'static str,
    pub features: String,
    pub start_unix_seconds: u64,
    pub models_loaded: usize,
    pub models_enabled_on_disk: usize,
    pub ready: bool,
    pub draining: bool,
    pub cluster_members: usize,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_started(&self, method: usize) {
        self.methods[method]
            .requests
            .fetch_add(1, Ordering::Relaxed);
        self.in_flight.fetch_add(1, Ordering::Relaxed);
    }

    /// A request that produced a response. Records latency; the histogram
    /// deliberately excludes failures, whose duration says more about when a
    /// guard fired than about how long inference takes.
    pub fn request_completed(&self, method: usize, items: usize, elapsed: Duration) {
        let m = &self.methods[method];
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        m.items.fetch_add(items as u64, Ordering::Relaxed);
        m.duration_nanos.fetch_add(
            elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        let secs = elapsed.as_secs_f64();
        for (i, bound) in DURATION_BUCKETS.iter().enumerate() {
            if secs <= *bound {
                m.buckets[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// A request that produced an error, labelled by its wire error code.
    pub fn request_failed(&self, method: usize, code: &str) {
        let idx = CODES.iter().position(|c| *c == code).unwrap_or(CODE_OTHER);
        self.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.methods[method].errors[idx].fetch_add(1, Ordering::Relaxed);
    }

    pub fn admin_loaded(&self, n: u64) {
        self.admin_loads.fetch_add(n, Ordering::Relaxed);
    }

    pub fn admin_unloaded(&self, n: u64) {
        self.admin_unloads.fetch_add(n, Ordering::Relaxed);
    }

    pub fn admin_refused(&self) {
        self.admin_refusals.fetch_add(1, Ordering::Relaxed);
    }

    pub fn config_served(&self) {
        self.config_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn warmup_failed(&self) {
        self.warmup_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(Ordering::Relaxed)
    }

    /// Render the Prometheus text exposition format (version 0.0.4).
    pub fn render(&self, snap: &Snapshot) -> String {
        let mut out = String::with_capacity(4096);

        out.push_str("# HELP postvec_server_build_info Build metadata; always 1.\n");
        out.push_str("# TYPE postvec_server_build_info gauge\n");
        out.push_str(&format!(
            "postvec_server_build_info{{version=\"{}\",features=\"{}\"}} 1\n",
            escape(snap.version),
            escape(&snap.features)
        ));

        gauge(
            &mut out,
            "postvec_server_start_time_seconds",
            "Unix time at which this process started serving.",
            snap.start_unix_seconds as f64,
        );
        gauge(
            &mut out,
            "postvec_server_ready",
            "1 when at least one model can answer and the node is not draining.",
            u8::from(snap.ready).into(),
        );
        gauge(
            &mut out,
            "postvec_server_draining",
            "1 after a shutdown signal, while in-flight work finishes.",
            u8::from(snap.draining).into(),
        );
        gauge(
            &mut out,
            "postvec_server_models_loaded",
            "Models resident in the engine.",
            snap.models_loaded as f64,
        );
        gauge(
            &mut out,
            "postvec_server_models_enabled_on_disk",
            "Enabled descriptors under the engine root. Above models_loaded means a pull or \
             activate has not been loaded yet.",
            snap.models_enabled_on_disk as f64,
        );
        gauge(
            &mut out,
            "postvec_server_cluster_members",
            "Alive peers in this node's group, this node included.",
            snap.cluster_members as f64,
        );
        gauge(
            &mut out,
            "postvec_server_requests_in_flight",
            "Inference requests currently executing.",
            self.in_flight() as f64,
        );
        gauge(
            &mut out,
            "postvec_server_warmup_failures_total",
            "Boot-time warmup predictions that failed.",
            self.warmup_failures.load(Ordering::Relaxed) as f64,
        );

        out.push_str("# HELP postvec_server_requests_total Inference requests accepted.\n");
        out.push_str("# TYPE postvec_server_requests_total counter\n");
        for (i, method) in METHODS.iter().enumerate() {
            out.push_str(&format!(
                "postvec_server_requests_total{{method=\"{method}\"}} {}\n",
                self.methods[i].requests.load(Ordering::Relaxed)
            ));
        }

        out.push_str(
            "# HELP postvec_server_request_errors_total Failed requests by wire error code.\n",
        );
        out.push_str("# TYPE postvec_server_request_errors_total counter\n");
        for (i, method) in METHODS.iter().enumerate() {
            for (c, code) in CODES.iter().enumerate() {
                let v = self.methods[i].errors[c].load(Ordering::Relaxed);
                if v > 0 {
                    out.push_str(&format!(
                        "postvec_server_request_errors_total{{method=\"{method}\",code=\"{code}\"}} {v}\n"
                    ));
                }
            }
        }

        out.push_str("# HELP postvec_server_items_total Texts embedded or embeddings converted.\n");
        out.push_str("# TYPE postvec_server_items_total counter\n");
        for (i, method) in METHODS.iter().enumerate() {
            out.push_str(&format!(
                "postvec_server_items_total{{method=\"{method}\"}} {}\n",
                self.methods[i].items.load(Ordering::Relaxed)
            ));
        }

        out.push_str(
            "# HELP postvec_server_request_duration_seconds Successful request latency.\n",
        );
        out.push_str("# TYPE postvec_server_request_duration_seconds histogram\n");
        for (i, method) in METHODS.iter().enumerate() {
            let m = &self.methods[i];
            for (b, bound) in DURATION_BUCKETS.iter().enumerate() {
                let le = if bound.is_infinite() {
                    "+Inf".to_string()
                } else {
                    format!("{bound}")
                };
                out.push_str(&format!(
                    "postvec_server_request_duration_seconds_bucket{{method=\"{method}\",le=\"{le}\"}} {}\n",
                    m.buckets[b].load(Ordering::Relaxed)
                ));
            }
            out.push_str(&format!(
                "postvec_server_request_duration_seconds_sum{{method=\"{method}\"}} {}\n",
                m.duration_nanos.load(Ordering::Relaxed) as f64 / 1e9
            ));
            out.push_str(&format!(
                "postvec_server_request_duration_seconds_count{{method=\"{method}\"}} {}\n",
                m.buckets[DURATION_BUCKETS.len() - 1].load(Ordering::Relaxed)
            ));
        }

        counter(
            &mut out,
            "postvec_server_admin_models_loaded_total",
            "Models loaded through the admin port.",
            self.admin_loads.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "postvec_server_admin_models_unloaded_total",
            "Models unloaded through the admin port.",
            self.admin_unloads.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "postvec_server_admin_refusals_total",
            "Admin requests refused (non-loopback peer, bad body, unknown model).",
            self.admin_refusals.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "postvec_server_config_requests_total",
            "GET /config discovery reads.",
            self.config_requests.load(Ordering::Relaxed),
        );

        out
    }
}

fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"
    ));
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    out.push_str(&format!(
        "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}\n"
    ));
}

fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', " ")
}

/// Compile-time execution providers, for `build_info` and `postvec-server --version`.
pub fn features() -> String {
    let mut features = vec!["onnx"];
    if cfg!(feature = "ort-cuda") {
        features.push("ort-cuda");
    }
    if cfg!(feature = "ort-tensorrt") {
        features.push("ort-tensorrt");
    }
    features.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> Snapshot {
        Snapshot {
            version: "0.1.0",
            features: "onnx".to_string(),
            start_unix_seconds: 1_700_000_000,
            models_loaded: 2,
            models_enabled_on_disk: 3,
            ready: true,
            draining: false,
            cluster_members: 1,
        }
    }

    /// The first thirteen labels must be exactly `shared::ErrorCode`, in
    /// order. That set is a pinned two-repo wire contract; a variant added
    /// privately and not mirrored here would silently land in `OTHER`.
    #[test]
    fn the_error_labels_cover_the_whole_wire_taxonomy() {
        use shared::ErrorCode::*;
        let taxonomy = [
            InternalError,
            InvalidInput,
            Timeout,
            ModelNotFound,
            ModelNotLoaded,
            ModelDisabled,
            ContextLengthExceeded,
            GpuOutOfMemory,
            CpuOverload,
            BridgePathNotFound,
            ConverterNotFound,
            TargetRestricted,
            UpstreamServiceUnavailable,
        ];
        for (i, code) in taxonomy.iter().enumerate() {
            assert_eq!(CODES[i], code.as_str(), "label {i}");
        }
        for code in taxonomy {
            assert!(
                CODES.iter().position(|c| *c == code.as_str()).unwrap() != CODE_OTHER,
                "{} must have its own series",
                code.as_str()
            );
        }
    }

    #[test]
    fn an_unknown_code_folds_into_other_instead_of_minting_a_series() {
        let m = Metrics::new();
        m.request_started(METHOD_EMBED);
        m.request_failed(METHOD_EMBED, "SOMETHING_NEW");
        let rendered = m.render(&snapshot());
        assert!(rendered.contains("code=\"OTHER\"} 1"), "{rendered}");
        assert!(!rendered.contains("SOMETHING_NEW"));
    }

    #[test]
    fn histogram_buckets_are_cumulative_and_counted() {
        let m = Metrics::new();
        m.request_started(METHOD_EMBED);
        m.request_completed(METHOD_EMBED, 4, Duration::from_millis(30));
        let out = m.render(&snapshot());
        // 30 ms lands above the 25 ms bound and inside the 50 ms one.
        assert!(
            out.contains("method=\"embed_texts\",le=\"0.025\"} 0"),
            "{out}"
        );
        assert!(
            out.contains("method=\"embed_texts\",le=\"0.05\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("method=\"embed_texts\",le=\"+Inf\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("postvec_server_request_duration_seconds_count{method=\"embed_texts\"} 1")
        );
        assert!(out.contains("postvec_server_items_total{method=\"embed_texts\"} 4"));
    }

    #[test]
    fn in_flight_returns_to_zero() {
        let m = Metrics::new();
        m.request_started(METHOD_CONVERT);
        assert_eq!(m.in_flight(), 1);
        m.request_completed(METHOD_CONVERT, 1, Duration::from_millis(1));
        assert_eq!(m.in_flight(), 0);

        m.request_started(METHOD_CONVERT);
        m.request_failed(METHOD_CONVERT, "TIMEOUT");
        assert_eq!(m.in_flight(), 0, "a failed request releases its slot too");
    }

    #[test]
    fn every_series_declares_help_and_type() {
        let out = Metrics::new().render(&snapshot());
        for line in out.lines().filter(|l| !l.starts_with('#')) {
            let name = line.split(['{', ' ']).next().unwrap();
            let base = name
                .trim_end_matches("_bucket")
                .trim_end_matches("_sum")
                .trim_end_matches("_count");
            assert!(
                out.contains(&format!("# TYPE {base} ")),
                "{name} has no TYPE line"
            );
        }
    }

    #[test]
    fn label_values_are_escaped() {
        let m = Metrics::new();
        let mut snap = snapshot();
        snap.features = "a\"b\\c".to_string();
        let out = m.render(&snap);
        assert!(out.contains(r#"features="a\"b\\c""#), "{out}");
    }
}
