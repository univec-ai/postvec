//! GUC definitions. Registered from `_PG_init`.

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use std::ffi::CString;

pub static NINFERENCE_GRPC_ENDPOINTS: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);
pub static NINFERENCE_HTTP_ENDPOINTS: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);
pub static DATABASE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static NOTIFY_ON_WRITE: GucSetting<bool> = GucSetting::<bool>::new(false);
pub static WORKER_ENABLED: GucSetting<bool> = GucSetting::<bool>::new(true);
pub static POLL_INTERVAL_MS: GucSetting<i32> = GucSetting::<i32>::new(5000);
pub static BATCH_SIZE: GucSetting<i32> = GucSetting::<i32>::new(64);
pub static MIGRATE_BATCH_SIZE: GucSetting<i32> = GucSetting::<i32>::new(256);
pub static EMBED_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(30_000);
pub static QUERY_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(2_000);
pub static MAX_RETRIES: GucSetting<i32> = GucSetting::<i32>::new(5);
pub static RETRY_BACKOFF_MS: GucSetting<i32> = GucSetting::<i32>::new(5_000);
pub static JOB_VISIBILITY_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(300_000);
pub static MODEL_REFRESH_INTERVAL_MS: GucSetting<i32> = GucSetting::<i32>::new(60_000);
pub static SEARCH_DEGRADE_TO_FTS: GucSetting<bool> = GucSetting::<bool>::new(true);
pub static DISCOVERY_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(5_000);
pub static WORKER_LOCK_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(10_000);
pub static MAX_DOCUMENT_BYTES: GucSetting<i32> = GucSetting::<i32>::new(1_048_576);
pub static MAX_BATCH_TOTAL_BYTES: GucSetting<i32> = GucSetting::<i32>::new(16_777_216);
pub static DDL_LOCK_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(60_000);
pub static HEARTBEAT_INTERVAL_MS: GucSetting<i32> = GucSetting::<i32>::new(30_000);

// Embedded-mode settings. All POSTMASTER: the engine, its model pools and
// its loopback listeners are built once at worker start and cannot be
// swapped in later. Like postvec.database they are only defined under
// shared_preload_libraries (a POSTMASTER GUC cannot be created from a
// plain backend load). Backends of a preloaded cluster inherit the values.
pub static MODE: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static NINFERENCE_PATH: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static EMBEDDED_MODELS: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static EMBEDDED_LISTEN: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static EMBEDDED_HTTP_LISTEN: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);
pub static EMBEDDED_MAX_INFLIGHT: GucSetting<i32> = GucSetting::<i32>::new(1);

/// Where the in-worker gRPC server listens when `postvec.embedded_listen` is
/// unset. Loopback, so source text never leaves the database host.
pub const DEFAULT_EMBEDDED_LISTEN: &str = "127.0.0.1:33433";

/// Where the engine host serves `GET /config` (model discovery for per-DB
/// workers and `refresh_models()`) when `postvec.embedded_http_listen` is
/// unset.
pub const DEFAULT_EMBEDDED_HTTP_LISTEN: &str = "127.0.0.1:33434";

/// The deployment mode selected by `postvec.mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Default: thin client to remote ninference nodes.
    Grpc,
    /// The background worker hosts the engine in-process.
    Embedded,
}

/// Strict parse for startup. The worker parks with a warning on an unknown
/// value; `mode()` is the lenient read used on hot paths.
pub fn parse_mode(raw: Option<&str>) -> Result<Mode, String> {
    match raw.map(str::trim) {
        None | Some("") | Some("grpc") => Ok(Mode::Grpc),
        Some("embedded") => Ok(Mode::Embedded),
        Some(other) => Err(format!(
            "invalid postvec.mode {other:?} (expected 'grpc' or 'embedded')"
        )),
    }
}

/// Strict read of `postvec.mode` (the worker parks on a bad value).
pub fn mode_checked() -> Result<Mode, String> {
    let raw = MODE.get().map(|c| c.to_string_lossy().to_string());
    parse_mode(raw.as_deref())
}

/// Current mode; unknown values fall back to `Grpc`. Backends stay lenient
/// so a typo does not fail every `search()`. The worker, which is the only
/// actor that drains row text, validates strictly at startup and parks on a
/// bad value.
pub fn mode() -> Mode {
    mode_checked().unwrap_or(Mode::Grpc)
}

/// The embedded listen address (with default applied).
pub fn embedded_listen() -> String {
    EMBEDDED_LISTEN
        .get()
        .map(|c| c.to_string_lossy().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_EMBEDDED_LISTEN.to_string())
}

/// The embedded HTTP `/config` listen address (with default applied).
pub fn embedded_http_listen() -> String {
    EMBEDDED_HTTP_LISTEN
        .get()
        .map(|c| c.to_string_lossy().trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_EMBEDDED_HTTP_LISTEN.to_string())
}

pub fn register() {
    GucRegistry::define_string_guc(
        c"postvec.ninference_grpc_endpoints",
        c"Comma-separated host:port list of ninference gRPC endpoints",
        c"Round-robined. Addresses are expected to be on a trusted private network (gRPC has no TLS).",
        &NINFERENCE_GRPC_ENDPOINTS,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"postvec.ninference_http_endpoints",
        c"Comma-separated http(s)://host:port list of ninference HTTP endpoints",
        c"Used only for GET /config model discovery.",
        &NINFERENCE_HTTP_ENDPOINTS,
        GucContext::Sighup,
        GucFlags::default(),
    );
    // PGC_POSTMASTER variables can only be created while
    // shared_preload_libraries is being processed; when the library is merely
    // loaded by CREATE EXTENSION in a backend, skip it — without preload there
    // is no background worker to consume it.
    if unsafe { pgrx::pg_sys::process_shared_preload_libraries_in_progress } {
        GucRegistry::define_string_guc(
            c"postvec.database",
            c"Comma-separated database(s) served by the postvec background workers",
            c"The launcher spawns (and respawns) one dynamic worker per listed database. \
              Unset: the launcher idles and logs a hint.",
            &DATABASE,
            GucContext::Postmaster,
            GucFlags::default(),
        );
        GucRegistry::define_string_guc(
            c"postvec.mode",
            c"Inference deployment mode: 'grpc' (remote ninference) or 'embedded'",
            c"'embedded' hosts the UniVec engine in-process (build with --features embedded): \
              the launcher hosts one shared engine and serves the per-database workers and \
              connection backends over loopback listeners; \
              postvec.ninference_*_endpoints are then ignored.",
            &MODE,
            GucContext::Postmaster,
            GucFlags::default(),
        );
        GucRegistry::define_string_guc(
            c"postvec.ninference_path",
            c"Engine root path for embedded mode (libs/, models/)",
            c"Falls back to the NINFERENCE_PATH environment variable when unset.",
            &NINFERENCE_PATH,
            GucContext::Postmaster,
            GucFlags::default(),
        );
        GucRegistry::define_string_guc(
            c"postvec.embedded_models",
            c"Comma-separated model names to preload at worker start (embedded mode)",
            c"Unset: every enabled model found under <root>/models is loaded.",
            &EMBEDDED_MODELS,
            GucContext::Postmaster,
            GucFlags::default(),
        );
        GucRegistry::define_string_guc(
            c"postvec.embedded_listen",
            c"host:port the in-worker gRPC server binds in embedded mode",
            c"Default 127.0.0.1:33433. Backends' search()/embed() (and, in launcher mode, \
              the per-database workers) dial this address; keep it on loopback — anything \
              that can reach it can drive inference.",
            &EMBEDDED_LISTEN,
            GucContext::Postmaster,
            GucFlags::default(),
        );
        GucRegistry::define_int_guc(
            c"postvec.embedded_max_inflight",
            c"Engine-wide cap on concurrently executing embedded predictions",
            c"Across all models and routes; the permit is held until the native computation \
              returns, so a timed-out caller cannot oversubscribe the database host. The \
              hard ceiling is deliberately small — embedded inference shares every core \
              with PostgreSQL.",
            &EMBEDDED_MAX_INFLIGHT,
            1,
            16,
            GucContext::Postmaster,
            GucFlags::default(),
        );
        GucRegistry::define_string_guc(
            c"postvec.embedded_http_listen",
            c"host:port the engine host serves GET /config on in embedded mode",
            c"Default 127.0.0.1:33434. Model discovery for per-database workers and \
              refresh_models(); same envelope shape as ninference's /config. Keep it on \
              loopback.",
            &EMBEDDED_HTTP_LISTEN,
            GucContext::Postmaster,
            GucFlags::default(),
        );
    }
    GucRegistry::define_bool_guc(
        c"postvec.notify_on_write",
        c"NOTIFY 'postvec' with the registry id after each write-back batch",
        c"Listeners receive one notification per completed batch at commit.",
        &NOTIFY_ON_WRITE,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"postvec.worker_enabled",
        c"Pause/resume background job processing without restart",
        c"",
        &WORKER_ENABLED,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.poll_interval_ms",
        c"Worker latch timeout when the queue is empty",
        c"Writers wake the worker through an at-commit latch nudge, so sync latency does \
          not ride this interval; the poll is the backstop for a worker that restarted \
          between the nudge being armed and the commit.",
        &POLL_INTERVAL_MS,
        10,
        3_600_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.batch_size",
        c"Texts per EmbedTexts call",
        c"",
        &BATCH_SIZE,
        1,
        4096,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.migrate_batch_size",
        c"Vectors per ConvertEmbeddings call",
        c"",
        &MIGRATE_BATCH_SIZE,
        1,
        4096,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.embed_timeout_ms",
        c"Worker-side gRPC deadline",
        c"",
        &EMBED_TIMEOUT_MS,
        100,
        3_600_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.query_timeout_ms",
        c"Backend-side deadline for search()/embed() inline inference",
        c"",
        &QUERY_TIMEOUT_MS,
        50,
        600_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.max_retries",
        c"Attempts before a job moves to postvec.jobs_dead",
        c"",
        &MAX_RETRIES,
        0,
        1000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.retry_backoff_ms",
        c"Base for exponential retry backoff",
        c"",
        &RETRY_BACKOFF_MS,
        0,
        3_600_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.job_visibility_timeout_ms",
        c"Stale claim reclamation window",
        c"",
        &JOB_VISIBILITY_TIMEOUT_MS,
        1000,
        86_400_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.model_refresh_interval_ms",
        c"GET /config poll cadence for the model cache",
        c"",
        &MODEL_REFRESH_INTERVAL_MS,
        1000,
        86_400_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"postvec.search_degrade_to_fts",
        c"Degrade search() to FTS-only with a WARNING when ninference is unreachable",
        c"",
        &SEARCH_DEGRADE_TO_FTS,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.worker_lock_timeout_ms",
        c"lock_timeout applied to every worker transaction (0 disables)",
        c"Bounds how long a blocked user table (e.g. an open ALTER TABLE) can stall the \
          worker; the failed batch is released with backoff instead of freezing the pipeline.",
        &WORKER_LOCK_TIMEOUT_MS,
        0,
        3_600_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.discovery_timeout_ms",
        c"Per-node HTTP timeout for GET /config model discovery",
        c"",
        &DISCOVERY_TIMEOUT_MS,
        100,
        600_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.max_document_bytes",
        c"Maximum UTF-8 bytes of one rendered document sent to inference",
        c"A row whose rendered text exceeds this dead-letters with the measured size (it is \
          never silently truncated: truncation would change embedding semantics and could \
          split UTF-8). Also bounds embed()/search() query inputs.",
        &MAX_DOCUMENT_BYTES,
        1_024,
        // Hard safety maximum: no supported model consumes anywhere near
        // 64 MiB of one document, and the ceiling is what bounds worker RSS.
        67_108_864,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.max_batch_total_bytes",
        c"Maximum summed rendered-text bytes per inference batch",
        c"Worker batches stop at this budget; rows past the boundary stay pending for the \
          next cycle without consuming a retry attempt. Bounds worker/backend memory and \
          the gRPC request size independently of postvec.batch_size row counts.",
        &MAX_BATCH_TOTAL_BYTES,
        65_536,
        // Hard safety maximum: a batch is held in memory twice (SPI read +
        // request build), so the ceiling caps worker RSS at a few hundred
        // MiB even when an operator raises the budget.
        268_435_456,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.ddl_lock_timeout_ms",
        c"lock_timeout ceiling applied inside postvec lifecycle functions (0 disables)",
        c"Applied via SET LOCAL before the first relation lock in enable/adopt/disable/\
          uninstall/set_format/migrate/migration_finalize/migration_abort/\
          create_vector_index/retry_dead, so a verb queued behind a long-running query \
          errors out instead of wedging all later queries behind its pending lock. A \
          caller's stricter (smaller, non-zero) lock_timeout is preserved. Note \
          lock_timeout bounds each acquisition, not total statement time.",
        &DDL_LOCK_TIMEOUT_MS,
        0,
        3_600_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"postvec.heartbeat_interval_ms",
        c"Maximum interval between worker heartbeat liveness writes",
        c"The heartbeat row is written when counters change, and otherwise at most once \
          per this interval — an idle worker no longer writes WAL every poll tick.",
        &HEARTBEAT_INTERVAL_MS,
        1_000,
        3_600_000,
        GucContext::Sighup,
        GucFlags::default(),
    );
}

/// Hard ceiling on configured endpoint/database list entries. More endpoints
/// than this cannot produce a bounded failover deadline or discovery fan-out.
/// Excess entries are returned in the rejected half of
/// [`parse_validated_list`]; the launcher warns per rejected database entry,
/// and `GrpcClient::from_gucs` warns once per distinct rejected endpoint set.
pub const MAX_LIST_ENTRIES: usize = 16;

/// Parse, trim, deduplicate (first occurrence wins, order preserved) and cap
/// a comma-separated GUC list. Entries longer than `max_len` bytes are
/// returned separately so the caller can warn without spamming hot paths.
pub fn parse_validated_list(raw: Option<CString>, max_len: usize) -> (Vec<String>, Vec<String>) {
    let mut seen = Vec::new();
    let mut rejected = Vec::new();
    for entry in parse_endpoint_list(raw) {
        if entry.len() > max_len {
            rejected.push(entry);
        } else if !seen.contains(&entry) {
            if seen.len() < MAX_LIST_ENTRIES {
                seen.push(entry);
            } else {
                rejected.push(entry);
            }
        }
    }
    (seen, rejected)
}

/// Parse a comma-separated GUC value into trimmed, non-empty strings.
pub fn parse_endpoint_list(raw: Option<CString>) -> Vec<String> {
    match raw {
        None => Vec::new(),
        Some(cs) => cs
            .to_string_lossy()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_endpoint_list, parse_mode, Mode};
    use std::ffi::CString;

    #[test]
    fn mode_parsing() {
        assert_eq!(parse_mode(None), Ok(Mode::Grpc));
        assert_eq!(parse_mode(Some("")), Ok(Mode::Grpc));
        assert_eq!(parse_mode(Some("grpc")), Ok(Mode::Grpc));
        assert_eq!(parse_mode(Some("embedded")), Ok(Mode::Embedded));
        assert_eq!(parse_mode(Some(" embedded ")), Ok(Mode::Embedded));
        assert!(parse_mode(Some("local")).is_err());
        assert!(parse_mode(Some("Embedded")).is_err(), "case-sensitive");
    }

    #[test]
    fn validated_list_dedupes_caps_and_rejects_overlong() {
        use super::{parse_validated_list, MAX_LIST_ENTRIES};
        let raw = CString::new(format!(
            "db1, db2, db1, {}, db3",
            "x".repeat(64) // one byte over the PostgreSQL identifier limit
        ))
        .unwrap();
        let (ok, rejected) = parse_validated_list(Some(raw), 63);
        assert_eq!(ok, vec!["db1", "db2", "db3"], "deduped, order preserved");
        assert_eq!(rejected, vec!["x".repeat(64)]);

        let many = (0..40)
            .map(|i| format!("db{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let (ok, rejected) = parse_validated_list(Some(CString::new(many).unwrap()), 63);
        assert_eq!(ok.len(), MAX_LIST_ENTRIES, "entry count is capped");
        assert_eq!(rejected.len(), 40 - MAX_LIST_ENTRIES);
    }

    #[test]
    fn endpoint_list_parsing() {
        assert_eq!(parse_endpoint_list(None), Vec::<String>::new());
        assert_eq!(
            parse_endpoint_list(Some(CString::new("").unwrap())),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_endpoint_list(Some(CString::new("192.0.2.1:33333").unwrap())),
            vec!["192.0.2.1:33333"]
        );
        assert_eq!(
            parse_endpoint_list(Some(
                CString::new(" 192.0.2.1:33333 , 192.0.2.2:33333,, ").unwrap()
            )),
            vec!["192.0.2.1:33333", "192.0.2.2:33333"]
        );
    }
}
