//! The postvec background worker.
//!
//! The worker's main thread strictly alternates SPI transactions with
//! `block_on`-driven network I/O. SPI is never interleaved inside async
//! code, and no thread other than the worker main thread ever touches
//! Postgres state (`crate::runtime`, `crate::jobs`).
//!
//! The static worker is always the launcher. It spawns one dynamic worker
//! per database listed in `postvec.database` and respawns any that stop;
//! each worker takes a session advisory lock so a respawn race can never
//! run two workers against one database. In embedded mode the launcher is
//! also the engine host: one shared engine plus loopback listeners, with
//! every per-database worker a thin loopback gRPC client (same as
//! connection backends).
//!
//! Each wake a worker refreshes the model cache on cadence, then drains
//! job batches (claim+read txn -> embed -> write-back txn), migration
//! batches ([`migrate`]) and cursor-backfill chunks ([`backfill`]), writing
//! a heartbeat + counters row each pass. Backends nudge the worker's latch
//! via `postvec.worker_kick()` (called from the generated triggers), so
//! typical sync latency is well under the poll interval.

pub mod backfill;
pub mod chunk;
pub mod index;
pub mod migrate;

use crate::{gucs, jobs};
use pgrx::bgworkers::{
    BackgroundWorker, BackgroundWorkerBuilder, DynamicBackgroundWorker, SignalWakeFlags,
};
use pgrx::pg_sys::panic::CaughtError;
use pgrx::prelude::*;
use pgrx::PgTryBuilder;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::time::{Duration, Instant};

/// Register the static worker (the launcher). Must be called from
/// `_PG_init`, only while shared_preload_libraries is being processed.
pub fn register_static_worker() {
    BackgroundWorkerBuilder::new("postvec launcher")
        .set_function("postvec_worker_main")
        .set_library("postvec")
        .enable_spi_access()
        .set_restart_time(Some(Duration::from_secs(5)))
        .load();
}

/// Counters a worker accumulates over its lifetime; surfaced through
/// `postvec.worker_heartbeat` / `postvec.stats()`. `Clone`/`PartialEq` back
/// the change-gated heartbeat: the row is written when these differ from the
/// last written snapshot, else at most once per
/// `postvec.heartbeat_interval_ms`.
#[derive(Default, Clone, PartialEq)]
pub struct Counters {
    pub embedded: i64,
    pub nulled: i64,
    pub retried: i64,
    pub dead: i64,
    pub converted: i64,
    pub skipped: i64,
    pub refreshes: i64,
    pub errors: i64,
    pub last_error: Option<String>,
    /// Local refresh phase output.
    pub documents_chunked: i64,
    pub chunks_created: i64,
}

impl Counters {
    fn jobs_done(&self) -> i64 {
        self.embedded + self.nulled
    }

    pub(crate) fn error(&mut self, msg: String) {
        self.errors += 1;
        self.last_error = Some(msg);
    }
}

/// Idle cadence of the auto-index reconciliation scan — the heartbeat
/// liveness interval, reusing an operator-visible knob rather than adding one.
fn auto_index_scan_interval() -> Duration {
    Duration::from_millis(gucs::HEARTBEAT_INTERVAL_MS.get().max(1_000) as u64)
}

fn poll_interval() -> Duration {
    Duration::from_millis(gucs::POLL_INTERVAL_MS.get().max(10) as u64)
}

pub(crate) fn shutdown_requested() -> bool {
    unsafe { pg_sys::ShutdownRequestPending != 0 }
}

fn reload_config_if_pending() {
    let got_sighup = BackgroundWorker::sighup_received();
    let pending = unsafe { pg_sys::ConfigReloadPending != 0 };
    if pending {
        unsafe {
            pg_sys::ConfigReloadPending = 0;
            pg_sys::ProcessConfigFile(pg_sys::GucContext::PGC_SIGHUP);
        }
        log!("postvec: worker reloaded postgresql.conf after SIGHUP");
    } else if got_sighup {
        log!("postvec: worker got SIGHUP with no pending config reload");
    }
}

/// Run one worker transaction with the failure policy every worker SPI path
/// needs:
///
/// - `SET LOCAL lock_timeout` (from `postvec.worker_lock_timeout_ms`) so one
///   blocked user table — an open `ALTER TABLE`, a long row-lock holder —
///   errors out instead of freezing the single worker thread indefinitely
///   (no heartbeat, no progress on other entries, SIGTERM unhonoured).
/// - Any Postgres error raised inside the transaction (a raising user
///   trigger, CHECK constraint, policy, or that lock timeout) is caught, the
///   transaction is aborted with the canonical backend recovery call, and the
///   error message is returned — instead of panicking the worker into its
///   5-second crash-respawn loop. Callers route the failure to
///   retry/backoff/dead-letter in a fresh transaction.
/// - The schema version is asserted **inside** the transaction, before the
///   caller's work runs — see [`guard_schema_version`]. The outer gate in the
///   worker loop decides whether to run at all; this one makes the decision
///   hold for the duration of the work.
pub(crate) fn try_transaction<R, F>(f: F) -> Result<R, String>
where
    F: FnOnce() -> R + UnwindSafe + RefUnwindSafe,
{
    transaction_with_guard(true, f)
}

/// A transaction that reads *only* system catalogs, without the schema guard.
///
/// Exactly one caller: [`catalog_version`], which is how the guard's own
/// question gets answered. Running the guard here would make every outcome
/// look like a guard failure — "the extension is absent" and "the extension is
/// version 0.1.0" would both arrive as the same opaque error, and the worker
/// could no longer tell "not installed yet" (normal, silent) from "installed
/// and mismatched" (loud, with the fix). It reads no postvec table, so there
/// is nothing for the guard to protect.
fn try_catalog_transaction<R, F>(f: F) -> Result<R, String>
where
    F: FnOnce() -> R + UnwindSafe + RefUnwindSafe,
{
    transaction_with_guard(false, f)
}

fn transaction_with_guard<R, F>(guarded: bool, f: F) -> Result<R, String>
where
    F: FnOnce() -> R + UnwindSafe + RefUnwindSafe,
{
    PgTryBuilder::new(move || {
        BackgroundWorker::transaction(move || {
            let lock_ms = gucs::WORKER_LOCK_TIMEOUT_MS.get();
            if lock_ms > 0 {
                Spi::run(&format!("SET LOCAL lock_timeout = {lock_ms}"))
                    .unwrap_or_else(|e| warning!("postvec: SET lock_timeout failed: {e}"));
            }
            if guarded {
                guard_schema_version()?;
            }
            Ok(f())
        })
    })
    .catch_others(|e| {
        let msg = caught_error_message(&e);
        // The canonical backend error-recovery step: abort and clean up
        // whatever transaction state the error left behind (pgrx already
        // flushed the error state when it converted the longjmp to a panic).
        unsafe {
            pg_sys::AbortCurrentTransaction();
        }
        Err(msg)
    })
    .execute()
}

/// Advisory-lock key shared by workers and extension upgrades.
///
/// `hashtext` is stable within a PostgreSQL major and the value only has to
/// agree between processes of the same cluster, which it does.
const SCHEMA_LOCK_SQL: &str =
    "SELECT pg_advisory_xact_lock_shared(hashtext('postvec_schema'))::text";

/// Refuse to touch postvec's tables unless the installed SQL is this library's.
///
/// This runs *inside* the caller's transaction, and in this order:
///
/// 1. take the schema advisory lock in **shared** mode;
/// 2. read `pg_extension.extversion`;
/// 3. compare it with the compiled-in version.
///
/// The order is the point. Checking the version in an earlier, separate
/// transaction — which is all the worker loop's outer gate can do — leaves a
/// window in which `ALTER EXTENSION postvec UPDATE` commits between the check
/// and the work, so an old worker writes rows into a new schema. Here the lock
/// is held for the rest of the transaction, so an upgrade running the
/// documented protocol (an exclusive `pg_advisory_xact_lock` at the top of
/// every upgrade script) either waits for this transaction to finish or
/// has already committed, in which case step 3 sees the new version and
/// refuses.
///
/// The failure is returned, not raised: callers already have a policy for a
/// failed transaction, and [`is_version_skew`] lets the drain loop recognise
/// this particular one and stop rather than burn job attempts on it.
fn guard_schema_version() -> Result<(), String> {
    guard_schema_version_against(env!("CARGO_PKG_VERSION"))
}

/// The guard, with the expected version as a parameter so both outcomes can be
/// exercised against a real catalog.
fn guard_schema_version_against(expected: &str) -> Result<(), String> {
    Spi::run(SCHEMA_LOCK_SQL).map_err(|e| format!("schema lock: {e}"))?;
    let installed = Spi::get_one::<String>(CATALOG_VERSION_SQL)
        .map_err(|e| format!("{VERSION_SKEW}: catalog unreadable: {e}"))?;
    match installed.as_deref() {
        Some(version) if version == expected => Ok(()),
        Some(version) => Err(format!(
            "{VERSION_SKEW}: installed extension is {version}, this library is {expected}"
        )),
        // The extension was dropped between the outer gate and here.
        None => Err(format!("{VERSION_SKEW}: the extension is not installed")),
    }
}

/// Prefix of the error [`guard_schema_version`] returns. A version skew is not
/// the job's fault, so the drain loop checks for it and stops instead of
/// letting the failure count against a job's retry budget.
pub(crate) const VERSION_SKEW: &str = "postvec: extension version skew";

pub(crate) fn is_version_skew(error: &str) -> bool {
    error.starts_with(VERSION_SKEW)
}

fn caught_error_message(e: &CaughtError) -> String {
    match e {
        CaughtError::PostgresError(er)
        | CaughtError::ErrorReport(er)
        | CaughtError::RustPanic { ereport: er, .. } => {
            format!("{:?}: {}", er.sql_error_code(), er.message())
        }
    }
}

/// The worker pid armed for an at-commit latch nudge in this backend, and
/// whether the transaction callback has been registered for this process.
/// The callback fires after the transaction is committed and visible,
/// once. A rollback disarms it. Setting the latch pre-commit would let
/// the worker wake, see nothing and wait out the poll interval; a
/// row-mode bulk statement would also set the latch once per row.
static KICK_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static KICK_CALLBACK_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// At-commit/abort hook: must not allocate, error, or touch the database —
/// it only reads process-shared state and sets a latch (async-signal-safe).
extern "C-unwind" fn kick_xact_callback(
    event: pg_sys::XactEvent::Type,
    _arg: *mut std::ffi::c_void,
) {
    use std::sync::atomic::Ordering;
    match event {
        pg_sys::XactEvent::XACT_EVENT_COMMIT | pg_sys::XactEvent::XACT_EVENT_PARALLEL_COMMIT => {
            let pid = KICK_PID.swap(0, Ordering::Relaxed);
            if pid > 0 {
                unsafe {
                    let proc = pg_sys::BackendPidGetProc(pid);
                    if !proc.is_null() {
                        pg_sys::SetLatch(&raw mut (*proc).procLatch);
                    }
                }
            }
        }
        pg_sys::XactEvent::XACT_EVENT_ABORT | pg_sys::XactEvent::XACT_EVENT_PARALLEL_ABORT => {
            KICK_PID.store(0, Ordering::Relaxed);
        }
        // Two-phase commit: the outcome is decided later — possibly in a
        // different backend via COMMIT/ROLLBACK PREPARED — so disarm rather
        // than leave the pid armed until this backend's next unrelated
        // commit. The poll interval is the delivery path for 2PC work.
        pg_sys::XactEvent::XACT_EVENT_PREPARE => {
            KICK_PID.store(0, Ordering::Relaxed);
        }
        _ => {}
    }
}

/// Nudge the background worker's latch so pending work is noticed immediately
/// instead of at the next poll tick. Called by the generated triggers,
/// `enable()`, and `migrate()`. Best-effort: silently a no-op when no worker
/// is running.
///
/// The nudge is **coalesced per transaction and delivered after commit**
/// through a transaction callback: however many rows a statement (or
/// transaction) touches, the backend performs one heartbeat lookup, arms the
/// pid, and the worker's latch is set exactly once — after the enqueued jobs
/// are visible, so the woken worker actually finds them. A rolled-back
/// transaction never wakes the worker. The poll interval remains the backstop
/// for a worker that restarts between arm and commit.
#[pg_extern]
pub(crate) fn worker_kick() {
    use std::sync::atomic::Ordering;
    if KICK_PID.load(Ordering::Relaxed) > 0 {
        return; // already armed by an earlier kick in this transaction
    }
    let pid = match Spi::get_one::<i32>("SELECT pid FROM postvec.worker_heartbeat LIMIT 1") {
        Ok(Some(pid)) if pid > 0 => pid,
        _ => return,
    };
    if !KICK_CALLBACK_REGISTERED.swap(true, Ordering::Relaxed) {
        unsafe {
            pg_sys::RegisterXactCallback(Some(kick_xact_callback), std::ptr::null_mut());
        }
    }
    KICK_PID.store(pid, Ordering::Relaxed);
}

/// The static worker is always the **launcher**: it spawns one dynamic
/// worker per database listed in `postvec.database` (one or many) and, in
/// embedded mode, hosts the one shared engine. Draining always happens in
/// the per-database dynamic workers — there is no separate single-database
/// code path.
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn postvec_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);

    let Some(mode) = mode_or_park("launcher") else {
        return;
    };

    // Validate the database list before any worker registration: pgrx stores
    // the name in fixed-size bgworker fields (96/128-byte C buffers), and a
    // real database name caps at 63 bytes (NAMEDATALEN - 1). An overlong or
    // duplicated entry must become a logged configuration error here — not a
    // truncated C string, a duplicate worker, or a respawn loop.
    let (databases, rejected) = gucs::parse_validated_list(gucs::DATABASE.get(), 63);
    for bad in &rejected {
        warning!(
            "postvec: ignoring postvec.database entry {:?} (over 63 bytes, or past the \
             {}-entry ceiling)",
            bad,
            gucs::MAX_LIST_ENTRIES
        );
    }
    // Charset guard: control characters, whitespace, quotes and backslashes
    // in a database name are far more likely a mangled GUC than a real
    // (exotic, quoted-identifier) database, and each one costs a
    // FATAL/quarantine cycle to discover. Reject loudly here instead.
    let (databases, invalid): (Vec<String>, Vec<String>) =
        databases.into_iter().partition(|name| {
            !name
                .chars()
                .any(|c| c.is_control() || c.is_whitespace() || c == '"' || c == '\\')
        });
    for bad in &invalid {
        warning!(
            "postvec: ignoring postvec.database entry {:?} (contains whitespace, \
             control characters, or quoting — quoted/exotic database names are not \
             supported here)",
            bad
        );
    }
    if databases.is_empty() {
        log!("postvec: launcher idle — set postvec.database to activate postvec");
        park_idle();
        return;
    }
    run_launcher(databases, mode);
}

/// Validate the deployment mode for a worker-family process, loudly. Parks
/// and returns `None` when the configuration cannot be served:
///
/// - An unparseable `postvec.mode` parks rather than guessing: falling back
///   to 'grpc' on a typo'd 'embedded' would silently ship row text over the
///   mesh — the exact intent violation the thin-build check also refuses.
///   (Backends' lenient `mode()` fallback is unaffected; workers are the
///   only actors that drain text.)
/// - A thin build (no `embedded` feature) asked to run embedded must not
///   silently drain over the mesh against operator intent — it parks.
fn mode_or_park(who: &str) -> Option<gucs::Mode> {
    let mode = match gucs::mode_checked() {
        Ok(mode) => mode,
        Err(e) => {
            warning!(
                "postvec: {e}; {who} idles until postvec.mode is fixed \
                 (set 'grpc' or 'embedded' and restart)"
            );
            park_idle();
            return None;
        }
    };
    if mode == gucs::Mode::Embedded && !cfg!(feature = "embedded") {
        warning!(
            "postvec: postvec.mode='embedded' but this postvec.so was built without the \
             'embedded' feature; {who} idles (rebuild with --features embedded or set \
             postvec.mode='grpc')"
        );
        park_idle();
        return None;
    }
    Some(mode)
}

/// Wait out the process lifetime doing nothing but signal bookkeeping (used
/// when configuration makes the worker unable to serve).
fn park_idle() {
    while BackgroundWorker::wait_latch(Some(Duration::from_secs(60))) {
        reload_config_if_pending();
        if shutdown_requested() {
            break;
        }
    }
}

/// Entry point for per-database dynamic workers; the database name arrives in
/// `bgw_extra` (set by the launcher).
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn postvec_worker_db_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    let db = BackgroundWorker::get_extra().to_string();
    if db.is_empty() {
        warning!("postvec: dynamic worker started without a database in bgw_extra; exiting");
        return;
    }
    let Some(mode) = mode_or_park(&format!("worker for db={db}")) else {
        return;
    };
    run_worker(&db, mode);
}

/// The launcher: spawn one dynamic worker per database and respawn any that
/// stop. Dynamic workers use BGW_NEVER_RESTART so ownership is unambiguous —
/// only the launcher (itself postmaster-restarted) respawns them; the
/// per-database advisory lock covers the launcher-crash race where a fresh
/// launcher spawns next to still-running old workers.
///
/// In embedded mode the launcher is additionally the **engine host**: the
/// one shared engine + the loopback gRPC and /config listeners live here,
/// and every per-database worker (like every connection backend) is a thin
/// loopback client. One engine, N databases — not RAM × N. Engine-init
/// failures degrade and retry (30 s backoff); they are visible in the server
/// log (the launcher serves no database, so there is no heartbeat row to
/// carry them — each worker's job errors surface transport failures in its
/// own `stats()` instead).
/// Quarantine ladder for per-database worker respawns. A worker that dies
/// fast (a missing/dropped database FATALs `connect_worker_to_spi` within
/// milliseconds) must not be respawned on a flat cadence forever.
/// Consecutive short-lived exits escalate 15 -> 30 -> 60 -> 120 -> 240 ->
/// 300 s (capped). A worker that survives past [`Self::HEALTHY_LIFETIME`]
/// resets the ladder, so a database created later recovers without a
/// restart. Pure, so the escalation sequence is deterministically
/// testable.
struct RespawnLadder {
    backoff: Duration,
}

impl RespawnLadder {
    const BASE: Duration = Duration::from_secs(15);
    const MAX: Duration = Duration::from_secs(300);
    const HEALTHY_LIFETIME: Duration = Duration::from_secs(30);

    fn new() -> Self {
        Self {
            backoff: Self::BASE,
        }
    }

    /// A worker exited after `lifetime`; returns the delay before the next
    /// spawn. Short-lived exits return the CURRENT rung and escalate for
    /// the next one; a healthy lifetime resets to the base.
    fn on_exit(&mut self, lifetime: Duration) -> Duration {
        if lifetime < Self::HEALTHY_LIFETIME {
            let delay = self.backoff;
            self.backoff = (self.backoff * 2).min(Self::MAX);
            delay
        } else {
            self.backoff = Self::BASE;
            Self::BASE
        }
    }
}

/// One per-database launcher slot: the spawn handle plus the respawn ladder
/// state. Module-level (not local to `run_launcher`) so the tick body can be
/// a named function behind the launcher's unwind boundary.
struct Slot {
    db: String,
    handle: Option<DynamicBackgroundWorker>,
    next_spawn: Instant,
    ladder: RespawnLadder,
    spawned_at: Option<Instant>,
}

fn run_launcher(databases: Vec<String>, mode: gucs::Mode) {
    let embedded = mode == gucs::Mode::Embedded;
    log!(
        "postvec: launcher started (pid={}, databases={databases:?}, embedded={embedded})",
        unsafe { pg_sys::MyProcPid }
    );

    let mut slots: Vec<Slot> = databases
        .into_iter()
        .map(|db| Slot {
            db,
            handle: None,
            next_spawn: Instant::now(),
            ladder: RespawnLadder::new(),
            spawned_at: None,
        })
        .collect();

    let mut panic_gate = Recurring::default();
    while BackgroundWorker::wait_latch(Some(Duration::from_secs(5))) {
        reload_config_if_pending();
        if shutdown_requested() {
            break;
        }
        // The launcher's unwind boundary, mirroring the per-database
        // workers' per-wake catch: a Rust panic in slot bookkeeping or the
        // engine-init plumbing must degrade to a rate-limited logged tick —
        // not a launcher FATAL, which in embedded mode tears down and
        // reloads every native model on respawn. (The launcher holds no
        // database connection, so there is no transaction to abort here.
        // Native aborts on engine/ORT/Rayon threads remain uncontainable.)
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            launcher_tick(&mut slots, embedded)
        })) {
            let msg = panic_message(payload);
            if panic_gate.should_report(&msg) {
                warning!("postvec: launcher tick panicked (recovered; next tick continues): {msg}");
            }
        }
    }

    // SIGTERM: take the fleet down with us.
    #[cfg(feature = "embedded")]
    if embedded {
        crate::client::embedded::shutdown();
    }
    for slot in slots {
        if let Some(handle) = slot.handle {
            let _ = handle.terminate();
        }
    }
    log!("postvec: launcher shutting down");
}

/// One launcher tick: engine hosting (embedded) plus slot spawn/respawn
/// bookkeeping. A named function so `run_launcher` can run it under its
/// unwind boundary.
fn launcher_tick(slots: &mut [Slot], embedded: bool) {
    // Engine hosting: rate-limited init attempts (maybe_init backs off
    // 30 s between failures and no-ops once ready). Workers are spawned
    // regardless — they gate draining on listener reachability, so
    // nothing burns job attempts while the engine is still coming up.
    if embedded {
        launcher_engine_tick();
    }
    for slot in slots.iter_mut() {
        if shutdown_requested() {
            break;
        }
        // Observe a death THE TICK it happens (≤ 5 s accuracy),
        // independently of the respawn schedule:
        // evaluating the lifetime only when next_spawn came due measured
        // "lifetime" as death-to-respawn wait included, so the second
        // observation of an instantly-dying worker landed exactly on the
        // healthy threshold and reset the ladder — a 15/30 s oscillation
        // that never escalated. NotYetStarted keeps the handle: the
        // registration is still queued at the postmaster, and spawning
        // again would duplicate it (the advisory lock is the last line
        // of defence, not the first).
        let stopped = matches!(
            slot.handle.as_ref().map(|h| h.pid()),
            Some(Err(pgrx::bgworkers::BackgroundWorkerStatus::Stopped
                | pgrx::bgworkers::BackgroundWorkerStatus::PostmasterDied))
        );
        if stopped {
            slot.handle = None;
            let lifetime = slot
                .spawned_at
                .map(|at| at.elapsed())
                .unwrap_or(Duration::ZERO);
            let escalating = lifetime < RespawnLadder::HEALTHY_LIFETIME;
            let delay = slot.ladder.on_exit(lifetime);
            slot.next_spawn = Instant::now() + delay;
            if escalating {
                warning!(
                    "postvec: worker for {:?} exited within {}s of spawn (missing \
                         database? failing startup?); quarantining the slot for {}s — \
                         it recovers automatically once the worker survives startup",
                    slot.db,
                    RespawnLadder::HEALTHY_LIFETIME.as_secs(),
                    delay.as_secs()
                );
            }
        }
        if slot.handle.is_some() || Instant::now() < slot.next_spawn {
            continue;
        }
        match spawn_db_worker(&slot.db) {
            Some(handle) => {
                log!("postvec: launcher spawned worker for {:?}", slot.db);
                slot.handle = Some(handle);
                slot.spawned_at = Some(Instant::now());
            }
            None => {
                slot.next_spawn = Instant::now() + RespawnLadder::BASE;
                warning!(
                    "postvec: launcher could not spawn a worker for {:?} \
                         (max_worker_processes exhausted?); retrying in {}s",
                    slot.db,
                    RespawnLadder::BASE.as_secs()
                )
            }
        }
    }
}

/// One rate-limited engine-init attempt from the launcher (a no-op once the
/// engine is up or while the retry backoff is pending). The launcher has no
/// database connection, so failures go to the server log only (workers
/// surface the downstream transport errors in their `stats()`).
#[cfg(feature = "embedded")]
fn launcher_engine_tick() {
    if let Err(e) = crate::client::embedded::maybe_init() {
        warning!("postvec: embedded engine init failed (will retry): {e}");
    }
}

#[cfg(not(feature = "embedded"))]
fn launcher_engine_tick() {}

fn spawn_db_worker(db: &str) -> Option<DynamicBackgroundWorker> {
    BackgroundWorkerBuilder::new(&format!("postvec worker [{db}]"))
        .set_function("postvec_worker_db_main")
        .set_library("postvec")
        .set_extra(db)
        .enable_spi_access()
        .set_restart_time(None) // launcher owns respawn
        .load_dynamic()
        .ok()
}

/// One latch wake of the per-database worker. Extracted so the caller can
/// wrap EVERY wake in a `catch_unwind`. A panic on the
/// refresh network path, a reachability probe, or any code outside a guarded
/// transaction must degrade to a logged, counted error — not a FATAL exit
/// into the launcher's respawn loop.
fn worker_wake(state: &mut WorkerState<'_>) -> WakeOutcome {
    {
        let db = state.db;
        let embedded = state.embedded;
        reload_config_if_pending();
        if shutdown_requested() {
            return WakeOutcome::Shutdown;
        }
        // Version gate: never touch postvec tables whose shape may belong to
        // another release (see `CatalogVersion`).
        match catalog_version() {
            CatalogVersion::Absent => {
                state.version_gate.clear();
                return WakeOutcome::Continue; // extension not present in this DB yet
            }
            CatalogVersion::Matched => state.version_gate.clear(),
            CatalogVersion::Mismatched(found) => {
                if state.version_gate.should_report(&found) {
                    warning!(
                        "postvec: worker parked for db={db}: the installed extension is version \
                         {found} but this postvec.so is {}; run ALTER EXTENSION postvec UPDATE \
                         (no jobs are processed until the versions agree)",
                        env!("CARGO_PKG_VERSION"),
                    );
                }
                return WakeOutcome::Continue;
            }
            CatalogVersion::Unreadable(e) => {
                if state.version_gate.should_report(&e) {
                    warning!("postvec: worker cannot read the installed extension version: {e}");
                }
                return WakeOutcome::Continue;
            }
        }

        // The model cache also serves backend-side resolution (search()'s
        // query embedding in both modes), so refresh it even while paused.
        maybe_refresh_models(
            db,
            embedded,
            &mut state.next_model_refresh,
            &mut state.counters,
            &mut state.hb,
            &mut state.http_gate,
        );

        if !gucs::WORKER_ENABLED.get() {
            write_heartbeat(&state.counters, state.started_at.as_deref(), &mut state.hb);
            return WakeOutcome::Continue;
        }

        // Nothing to do without an inference path (no endpoints configured /
        // engine host unreachable); don't burn job attempts. The auto-index
        // step still runs: an observed/no-backfill entry needs no inference
        // to receive its index, and eligibility (no pending or claimed job)
        // keeps entries with real queue work out of it.
        //
        // Local refresh and recursive-only cursor feeding also run behind
        // the closed gate, so chunk text and lexical search converge while
        // inference is down. Column-mode cursor backfill deliberately does
        // not: advancing its watermark and filling the queue while inference
        // is down would change shipped behaviour. Refresh expansion
        // self-bounds at CHUNK_INFLIGHT_MAX child jobs, so this local loop
        // terminates.
        if state.gate.closed(embedded) {
            let mut auto_any = false;
            loop {
                if shutdown_requested() {
                    return WakeOutcome::Shutdown;
                }
                reload_config_if_pending();
                if !gucs::WORKER_ENABLED.get() || !state.gate.closed(embedded) {
                    break;
                }
                // The guarded summary probe also carries the version gate: a
                // skew comes back as an error and stops the loop promptly.
                let summary = match work_summary() {
                    Ok(s) => s,
                    Err(e) if is_version_skew(&e) => break,
                    Err(e) => {
                        state.counters.error(format!("work summary: {e}"));
                        warning!("postvec: work summary probe failed (will retry): {e}");
                        break;
                    }
                };
                auto_any = summary.auto_index_any;
                let refreshed = summary.refresh_due && chunk::step(&mut state.counters);
                let enqueued = if summary.cursor_recursive_any {
                    backfill::step_recursive_only()
                } else {
                    0
                };
                write_heartbeat(&state.counters, state.started_at.as_deref(), &mut state.hb);
                if !refreshed && enqueued == 0 {
                    break;
                }
            }
            if auto_any && Instant::now() >= state.next_auto_index_scan {
                index::step(&mut state.counters);
                state.next_auto_index_scan = Instant::now() + auto_index_scan_interval();
            }
            write_heartbeat(&state.counters, state.started_at.as_deref(), &mut state.hb);
            return WakeOutcome::Continue;
        }

        // Drain: local refreshes, jobs, migration batches, and cursor-
        // backfill chunks, until a full pass yields no work (or we're asked
        // to stop). Failed work gets a future not_before / an error mark, so
        // the loop converges. A large backlog can keep this loop busy for a
        // long time, so the heartbeat, SIGHUP config reloads, the
        // `worker_enabled` pause switch, and the endpoint gate are all
        // serviced per iteration, not once per latch wake, so a large
        // backlog cannot starve them.
        let mut auto_any = false;
        loop {
            if shutdown_requested() {
                return WakeOutcome::Shutdown;
            }
            reload_config_if_pending();
            if !gucs::WORKER_ENABLED.get() || state.gate.closed(embedded) {
                break;
            }
            // One guarded summary probe decides which phases run this
            // iteration (an idle wake is exactly one transaction). It also
            // carries the version gate: draining a large backlog can run for
            // minutes, `ALTER EXTENSION` can commit at any point during it,
            // and the skew error here is what stops the loop promptly
            // instead of failing transaction after transaction.
            let summary = match work_summary() {
                Ok(s) => s,
                Err(e) if is_version_skew(&e) => break,
                Err(e) => {
                    state.counters.error(format!("work summary: {e}"));
                    warning!("postvec: work summary probe failed (will retry): {e}");
                    break;
                }
            };
            auto_any = summary.auto_index_any;
            // The refresh flag joins the break condition. Dropping it
            // would fall back to one document per latch wake.
            // `claimed_any` keeps crash recovery working: stale claims are
            // reclaimed by the claim path's probe even with nothing due.
            let refreshed = summary.refresh_due && chunk::step(&mut state.counters);
            let claimed =
                (summary.embed_due || summary.claimed_any) && run_one_cycle(&mut state.counters);
            let migrated = summary.migration_due && migrate::drain_step(&mut state.counters);
            let enqueued = if summary.cursor_any {
                backfill::step()
            } else {
                0
            };
            write_heartbeat(&state.counters, state.started_at.as_deref(), &mut state.hb);
            if !refreshed && !claimed && !migrated && enqueued == 0 {
                break;
            }
        }
        // At most one automatic index build per wake, only after the
        // drain pass above yielded no work (per-entry drain is additionally
        // enforced by the step's own no-pending-or-claimed-job eligibility).
        // The reconciliation scan is purely time-gated — at most once per
        // heartbeat interval on busy AND idle workers alike. It is an O(n)
        // pass over every active `auto` entry (per-entry catalog identity
        // checks included), so running it after every productive drain pass
        // made it the dominant per-wake cost under sustained load; the price
        // of the gate is an auto build starting up to one heartbeat interval
        // after its entry drains, which is immaterial next to build time.
        if auto_any && Instant::now() >= state.next_auto_index_scan {
            index::step(&mut state.counters);
            state.next_auto_index_scan = Instant::now() + auto_index_scan_interval();
        }
        write_heartbeat(&state.counters, state.started_at.as_deref(), &mut state.hb);
        WakeOutcome::Continue
    }
}

/// Per-worker mutable state, bundled so [`worker_wake`] can sit behind one
/// unwind boundary.
struct WorkerState<'a> {
    db: &'a str,
    embedded: bool,
    counters: Counters,
    started_at: Option<String>,
    next_model_refresh: Instant,
    version_gate: Recurring,
    hb: HeartbeatState,
    gate: GateCache,
    /// TTL cache over the embedded HTTP `/config` reachability probe.
    http_gate: GateCache,
    /// The auto-index reconciliation scan is purely time-gated: at most
    /// once per heartbeat interval, on busy and idle workers alike (an
    /// identity scan of every active `auto` entry per wake was the dominant
    /// per-wake cost under sustained load). The price is an auto build
    /// starting up to one interval after its entry drains.
    next_auto_index_scan: Instant,
}

enum WakeOutcome {
    Continue,
    Shutdown,
}

/// Best-effort text of a caught panic payload.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// The actual worker loop, one instance per database. Workers never host
/// the engine — in embedded mode they are thin clients of the
/// launcher-hosted loopback listeners, exactly like connection backends.
/// The caller has already validated the mode via [`mode_or_park`].
fn run_worker(db: &str, mode: gucs::Mode) {
    let embedded = mode == gucs::Mode::Embedded;

    BackgroundWorker::connect_worker_to_spi(Some(db), None);

    // Single-ownership guard: a session advisory lock keyed per database
    // (advisory locks are tagged with the database OID, so the same key in
    // different databases never collides). Held until process exit.
    // Catching wrapper: a raised ERROR in either startup
    // transaction must exit this worker cleanly (launcher respawns with
    // backoff) instead of escaping as an uncaught panic. Unguarded on
    // purpose — the extension may not be installed yet.
    let got_lock = try_catalog_transaction(|| {
        Spi::get_one::<bool>("SELECT pg_try_advisory_lock(hashtext('postvec_worker'), 0)")
            .unwrap_or(None)
            .unwrap_or(false)
    })
    .unwrap_or(false);
    if !got_lock {
        warning!(
            "postvec: another postvec worker already serves db={db}; this one exits \
             (launcher respawn race — harmless)"
        );
        return;
    }

    log!("postvec: worker started (db={db}, pid={})", unsafe {
        pg_sys::MyProcPid
    });

    let counters = Counters::default();
    let started_at: Option<String> =
        try_catalog_transaction(|| Spi::get_one::<String>("SELECT now()::text").ok().flatten())
            .unwrap_or(None);
    // First refresh lands after a small stable per-database delay (0-15 s)
    // instead of immediately. At cluster startup that keeps every worker
    // from firing its first discovery poll at once (D databases x 4
    // concurrent connections in one burst). The persisted model cache
    // covers resolution in the meantime. Scheduling is an explicit future
    // Instant, additions only: a subtraction-based form underflows with a
    // 1 s refresh interval and a 0-15 s startup delay.
    let next_model_refresh = Instant::now() + startup_refresh_delay(db);
    let version_gate = Recurring::default();
    let hb = HeartbeatState::default();
    let gate = GateCache::default();

    let mut state = WorkerState {
        db,
        embedded,
        counters,
        started_at,
        next_model_refresh,
        version_gate,
        hb,
        gate,
        http_gate: GateCache::default(),
        next_auto_index_scan: Instant::now(),
    };
    let mut panic_gate = Recurring::default();
    while BackgroundWorker::wait_latch(Some(poll_interval())) {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker_wake(&mut state)));
        match outcome {
            Ok(WakeOutcome::Shutdown) => break,
            Ok(WakeOutcome::Continue) => {}
            Err(payload) => {
                // Outermost unwind boundary: recover, count, rate-limit the
                // log, and clean up any transaction the panic escaped from —
                // the next wake starts fresh.
                let msg = panic_message(payload);
                state.counters.error(format!("worker wake panicked: {msg}"));
                if panic_gate.should_report(&msg) {
                    warning!(
                        "postvec: worker wake for db={} panicked (recovered; next wake \
                         continues): {msg}",
                        state.db
                    );
                }
                unsafe {
                    if pg_sys::IsTransactionState() {
                        pg_sys::AbortCurrentTransaction();
                    }
                }
                if shutdown_requested() {
                    break;
                }
            }
        }
    }
    let counters = state.counters;

    log!(
        "postvec: worker shutting down (embedded={}, converted={}, errors={})",
        counters.embedded,
        counters.converted,
        counters.errors
    );
}

/// One cheap guarded probe answering "which phases have work right now?", so
/// an idle tick costs one transaction instead of one per phase. Without
/// this, empty chunk-claim, job-claim, migration-scan, backfill-scan and
/// index-scan transactions would run every second on a completely idle
/// database. Every EXISTS rides an existing partial/small index; each
/// selected phase still runs in its own transaction so per-phase error
/// isolation is unchanged.
#[derive(Default, Clone, Copy)]
struct WorkSummary {
    refresh_due: bool,
    embed_due: bool,
    claimed_any: bool,
    migration_due: bool,
    cursor_any: bool,
    cursor_recursive_any: bool,
    auto_index_any: bool,
}

fn work_summary() -> Result<WorkSummary, String> {
    let out = try_transaction(|| -> Result<WorkSummary, String> {
        Spi::connect(|c| {
            let t = c
                .select(
                    "SELECT
                       EXISTS (SELECT 1 FROM postvec.jobs
                                WHERE claimed_at IS NULL AND not_before <= now()
                                  AND op = 'refresh'),
                       EXISTS (SELECT 1 FROM postvec.jobs
                                WHERE claimed_at IS NULL AND not_before <= now()
                                  AND op = 'embed'),
                       EXISTS (SELECT 1 FROM postvec.jobs WHERE claimed_at IS NOT NULL),
                       EXISTS (SELECT 1 FROM postvec.migrations
                                WHERE state = 'running' AND not_before <= now()),
                       EXISTS (SELECT 1 FROM postvec.registry
                                WHERE backfill_mode = 'cursor' AND state = 'active'),
                       EXISTS (SELECT 1 FROM postvec.registry
                                WHERE backfill_mode = 'cursor' AND state = 'active'
                                  AND chunking = 'recursive'),
                       EXISTS (SELECT 1 FROM postvec.registry
                                WHERE state = 'active' AND index_mode = 'auto')",
                    Some(1),
                    &[],
                )
                .map_err(|e| format!("work summary select: {e}"))?;
            let row = t
                .into_iter()
                .next()
                .ok_or_else(|| "work summary returned no row".to_string())?;
            // A column that fails to decode is a typed error, not a silent
            // `false`: coercing would quietly skip work phases for as long
            // as the failure persisted, with nothing in the log.
            let get = |i: usize| -> Result<bool, String> {
                row.get::<bool>(i)
                    .map_err(|e| format!("work summary column {i}: {e}"))?
                    .ok_or_else(|| format!("work summary column {i} was NULL"))
            };
            Ok(WorkSummary {
                refresh_due: get(1)?,
                embed_due: get(2)?,
                claimed_any: get(3)?,
                migration_due: get(4)?,
                cursor_any: get(5)?,
                cursor_recursive_any: get(6)?,
                auto_index_any: get(7)?,
            })
        })
    });
    match out {
        Ok(inner) => inner,
        Err(e) => Err(e),
    }
}

/// A short-TTL cache over [`inference_gate_closed`] so one wake performs at
/// most one loopback TCP probe. Without the cache the embedded gate would
/// be probed twice per idle tick: a connect/RST pair per second that also
/// woke the engine runtime.
#[derive(Default)]
struct GateCache {
    last: Option<(Instant, bool)>,
}

impl GateCache {
    const TTL: Duration = Duration::from_millis(1_000);

    /// Run `probe` at most once per TTL; in between, return the cached
    /// answer. Shared by the gRPC inference gate and the HTTP discovery
    /// reachability check (the latter must not probe uncached).
    fn check(&mut self, probe: impl FnOnce() -> bool) -> bool {
        if let Some((at, v)) = self.last {
            if at.elapsed() < Self::TTL {
                return v;
            }
        }
        let v = probe();
        self.last = Some((Instant::now(), v));
        v
    }

    fn closed(&mut self, embedded: bool) -> bool {
        self.check(|| inference_gate_closed(embedded))
    }
}

/// Whether the worker currently has no way to run inference: mode-aware —
/// gRPC mode needs configured endpoints; embedded mode needs the launcher's
/// loopback listener to be accepting.
fn inference_gate_closed(embedded: bool) -> bool {
    if embedded {
        // The engine lives in the launcher process. Probe the loopback
        // listener so jobs don't burn retry attempts while the engine host
        // is down or still loading models — the gRPC listener binds only
        // after the engine is built, so "accepting" implies "ready".
        return !tcp_reachable(&gucs::embedded_listen());
    }
    grpc_endpoints_empty()
}

/// Cheap synchronous reachability probe for a loopback listener. An
/// unparseable address counts as unreachable (the engine host would have
/// failed to bind it too).
fn tcp_reachable(addr: &str) -> bool {
    let Ok(addr) = addr.parse::<std::net::SocketAddr>() else {
        return false;
    };
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok()
}

/// One claim→embed→write-back pass over a single batch. Returns whether any
/// jobs were claimed (i.e. whether the queue may still hold work).
fn run_one_cycle(counters: &mut Counters) -> bool {
    let batch = gucs::BATCH_SIZE.get().max(1);
    let vis_secs = gucs::JOB_VISIBILITY_TIMEOUT_MS.get().max(1000) as f64 / 1000.0;
    let embed_timeout = gucs::EMBED_TIMEOUT_MS.get().max(100) as u64;
    let max_retries = gucs::MAX_RETRIES.get();
    let backoff_ms = gucs::RETRY_BACKOFF_MS.get().max(0) as i64;

    // txn A: claim + read source texts + capture routing (commits before the
    // network call). An aborted read (e.g. lock_timeout on a blocked table)
    // rolls the claims back with the transaction — nothing is lost; retry on
    // the next pass.
    let groups = match try_transaction(|| jobs::claim_and_read(batch, vis_secs)) {
        Ok(groups) => groups,
        Err(e) => {
            counters.error(format!("claim/read: {e}"));
            warning!("postvec: claim/read transaction failed (will retry): {e}");
            return false;
        }
    };
    if groups.is_empty() {
        return false;
    }

    // One transport for both modes: mesh endpoints in gRPC mode, the
    // launcher's loopback listener in embedded mode (from_gucs resolves it).
    let client = crate::client::grpc::GrpcClient::from_gucs(embed_timeout);
    let request_timeout = client.overall_timeout_ms();

    for group in groups {
        if shutdown_requested() {
            // Unprocessed claims re-deliver via the visibility timeout.
            return true;
        }
        let entry = group.entry;
        let routing = group.routing;
        let (null_jobs, embed_jobs) = jobs::split_items(group.items);
        let group_ids: Vec<i64> = null_jobs
            .iter()
            .map(|(id, _)| *id)
            .chain(embed_jobs.iter().map(|(id, _, _, _, _)| *id))
            .collect();

        // Resolve public model -> embed call (SPI, own txn). During a
        // migration the routing carries the NEW model. A model with no embed
        // model of its own rides an embed-bridge route (convert-only targets).
        let model = routing.model.clone();
        let resolved = try_transaction(move || crate::api::embed::resolve_embed_route(&model));

        // Network phase (no transaction).
        let outcomes = match resolved {
            Ok(Ok(resolution)) => {
                let (model, route) = resolution.into_call();
                let texts: Vec<String> =
                    embed_jobs.iter().map(|(_, _, _, _, t)| t.clone()).collect();
                // Sub-batched by the routing dimension so no single response
                // can exceed the transport's decode ceiling.
                jobs::embed_with_bisection_bounded(
                    &client,
                    &model,
                    &route,
                    request_timeout,
                    &texts,
                    routing.dim,
                )
            }
            Ok(Err(e)) => jobs::all_retry(embed_jobs.len(), &format!("model resolve: {e}")),
            Err(e) => jobs::all_retry(embed_jobs.len(), &format!("model resolve: {e}")),
        };

        // txn C: write-back / retry / dead-letter (re-verifies the routing).
        // If the whole transaction aborts — a raising user trigger, CHECK
        // constraint, policy, or lock timeout on the target table — release
        // the group in a fresh transaction (backoff, dead-letter once
        // attempts exhaust) instead of crash-looping the worker.
        let apply_result = {
            let routing = routing.clone();
            try_transaction(move || {
                jobs::apply_group(
                    entry.id,
                    &routing,
                    &null_jobs,
                    &embed_jobs,
                    &outcomes,
                    max_retries,
                    backoff_ms,
                )
            })
        };
        let applied = match apply_result {
            Ok(applied) => applied,
            Err(e) => {
                counters.error(format!("write-back: {e}"));
                warning!(
                    "postvec: write-back for {}.{}.{} failed ({e}); releasing the batch",
                    entry.table_schema,
                    entry.table_name,
                    entry.source_column
                );
                let recovered = try_transaction(move || {
                    jobs::release_failed_group(&group_ids, max_retries, backoff_ms, &e)
                });
                match recovered {
                    Ok(applied) => applied,
                    Err(e2) => {
                        // Even the recovery transaction failed (postvec's own
                        // tables should never do this): give up on the batch;
                        // the visibility timeout re-delivers it.
                        warning!("postvec: batch release failed too: {e2}");
                        jobs::Applied::default()
                    }
                }
            }
        };
        counters.embedded += applied.done;
        counters.nulled += applied.nulled;
        counters.retried += applied.retried;
        counters.dead += applied.dead;
        // `errors` counts real failures (dead-letters); retries/routing
        // releases are visible via `jobs_retried` and are not alert-worthy.
        counters.errors += applied.dead;

        if applied.done + applied.nulled + applied.retried + applied.dead > 0 {
            log!(
                "postvec: {}.{}.{} — {} embedded, {} nulled, {} retried, {} dead (model={})",
                entry.table_schema,
                entry.table_name,
                entry.source_column,
                applied.done,
                applied.nulled,
                applied.retried,
                applied.dead,
                routing.model,
            );
        }
    }
    true
}

/// What the database's `pg_extension` row says about postvec, relative to the
/// version of *this* library.
///
/// The gap this closes: `apt upgrade` (or any file-level install) replaces
/// `postvec.so` and a restart maps the new image, but the SQL side only moves
/// when someone runs `ALTER EXTENSION postvec UPDATE` in each database. In
/// that window a new worker would be reading and writing control tables whose
/// shape belongs to the previous release. So the worker parks instead: no
/// claims, no write-backs, no migration steps — and no heartbeat either, since
/// `postvec.worker_heartbeat` is one of the tables whose shape is in question.
/// The condition is visible in the server log and in `postvec doctor`'s
/// `extension.version` check.
#[derive(Debug, PartialEq, Eq)]
enum CatalogVersion {
    /// No `postvec` row in `pg_extension` — `CREATE EXTENSION` has not run in
    /// this database yet. Not an error: the worker polls until it appears.
    Absent,
    /// The installed SQL matches this library exactly.
    Matched,
    /// Installed SQL is some other version (carries it, for the message).
    Mismatched(String),
    /// The catalog could not be read at all (carries the error text).
    Unreadable(String),
}

/// The SQL that answers both questions the gate asks — "is it installed?" and
/// "which version?" — in one catalog lookup.
const CATALOG_VERSION_SQL: &str = "SELECT extversion FROM pg_extension WHERE extname = 'postvec'";

fn catalog_version() -> CatalogVersion {
    // Unguarded on purpose: this *is* the guard's question. See
    // `try_catalog_transaction`.
    let read = try_catalog_transaction(|| {
        Spi::get_one::<String>(CATALOG_VERSION_SQL).map_err(|e| e.to_string())
    });
    classify_catalog_version(read.unwrap_or_else(Err))
}

fn classify_catalog_version(read: Result<Option<String>, String>) -> CatalogVersion {
    match read {
        Ok(Some(found)) if found == env!("CARGO_PKG_VERSION") => CatalogVersion::Matched,
        Ok(Some(found)) => CatalogVersion::Mismatched(found),
        Ok(None) => CatalogVersion::Absent,
        Err(e) => CatalogVersion::Unreadable(e),
    }
}

/// A condition that is re-observed every poll tick but must not be logged
/// every poll tick. Reports the first sighting of a state, any change of
/// state, and then at most once per [`Recurring::REPEAT`].
#[derive(Default)]
struct Recurring {
    state: Option<(String, Instant)>,
}

impl Recurring {
    const REPEAT: Duration = Duration::from_secs(300);

    fn should_report(&mut self, state: &str) -> bool {
        let due = match &self.state {
            Some((seen, at)) => seen != state || at.elapsed() >= Self::REPEAT,
            None => true,
        };
        if due {
            self.state = Some((state.to_string(), Instant::now()));
        }
        due
    }

    /// The condition cleared: report it again next time it appears.
    fn clear(&mut self) {
        self.state = None;
    }
}

fn grpc_endpoints_empty() -> bool {
    gucs::parse_validated_list(gucs::GRPC_ENDPOINTS.get(), 512)
        .0
        .is_empty()
}

/// A stable per-database offset added to the refresh interval so D databases
/// on one cluster (or many clusters sharing a mesh) do not poll every
/// discovery endpoint in synchronized D × E bursts. FNV-1a over the database
/// name, folded into [0, interval/4).
fn refresh_jitter(db: &str, interval: Duration) -> Duration {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in db.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x1_0000_01b3);
    }
    let span_ms = (interval.as_millis() as u64 / 4).max(1);
    Duration::from_millis(h % span_ms)
}

/// The stable per-database delay of the FIRST refresh after worker start,
/// [0, 15 s) — deliberately independent of `model_refresh_interval_ms` so a
/// 1 s interval still spreads a cluster-wide start.
fn startup_refresh_delay(db: &str) -> Duration {
    refresh_jitter(db, Duration::from_secs(60))
}

/// Refresh the model cache when the scheduled `next_refresh` instant has
/// passed (steady-state cadence: `model_refresh_interval_ms` plus the
/// per-database jitter). Failures are logged, not fatal (the worker keeps
/// running on a stale cache). A completed refresh arms the heartbeat's
/// `models_refreshed_at` stamp — the diff-aware cache upsert no longer
/// rewrites unchanged rows, so freshness lives on the heartbeat.
///
/// In embedded mode discovery rides the launcher's loopback `/config`
/// listener (resolved by `GrpcClient::from_gucs`) — exactly like gRPC-mode
/// discovery against a one-node mesh, and complete by definition, so cache
/// pruning is safe.
fn maybe_refresh_models(
    db: &str,
    embedded: bool,
    next_refresh: &mut Instant,
    counters: &mut Counters,
    hb: &mut HeartbeatState,
    http_gate: &mut GateCache,
) {
    if Instant::now() < *next_refresh {
        return;
    }
    if embedded {
        // Skip quietly — and *without* rescheduling — while the engine host
        // is down: the first refresh must land right after the listener
        // comes up, not one interval later. The reachability gate already
        // reports why nothing drains. TTL-cached like the gRPC gate.
        if http_gate.check(|| !tcp_reachable(&gucs::embedded_http_listen())) {
            return;
        }
    } else if gucs::parse_validated_list(gucs::HTTP_ENDPOINTS.get(), 512)
        .0
        .is_empty()
    {
        // Discovery rides the HTTP endpoints, not the gRPC ones.
        return;
    }
    let base = Duration::from_millis(gucs::MODEL_REFRESH_INTERVAL_MS.get().max(1000) as u64);
    *next_refresh = Instant::now() + base + refresh_jitter(db, base);
    match crate::api::embed::list_models_report_blocking() {
        Ok(report) => {
            let n = report.models.len();
            // Guarded transactions: a PostgreSQL ERROR here (constraint,
            // disk-full, catalog drift) must degrade to a logged stale
            // cache like every other worker phase, not escape as an
            // uncaught panic that FATALs the worker into a respawn.
            let res = try_transaction(|| crate::api::embed::upsert_models(&report.models))
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            match res {
                Ok(()) => {
                    // `models_refreshed_at` stamps only a COMPLETE, fully
                    // successful refresh (every node answered AND the prune
                    // committed) — a partial fan-out or a failed prune must
                    // age the freshness signal, not mask itself as healthy.
                    let mut complete_success = false;
                    if report.complete {
                        match try_transaction(|| {
                            crate::api::embed::prune_unseen_models(&report.models)
                        })
                        .map_err(|e| e.to_string())
                        .and_then(|r| r.map_err(|e| e.to_string()))
                        {
                            Ok(()) => complete_success = true,
                            Err(e) => {
                                counters.error(format!("model-cache prune: {e}"));
                                warning!("postvec: worker model-cache prune failed: {e}");
                            }
                        }
                    } else {
                        warning!(
                            "postvec: worker model refresh reached {}/{} nodes; keeping stale cache rows",
                            report.ok_nodes,
                            report.ok_nodes + report.failed_nodes
                        );
                    }
                    // The counter and `models_refreshed_at` agree: both mean
                    // a COMPLETE refresh. A partial fan-out already warned
                    // above and ages the freshness signal; counting it too
                    // would let the two heartbeat fields contradict each
                    // other about refresh health.
                    if complete_success {
                        counters.refreshes += 1;
                        hb.stamp_refresh = true;
                    }
                    // debug1, not LOG: a healthy refresh every interval is
                    // steady-state noise; failures above stay loud.
                    pgrx::debug1!("postvec: worker refreshed model cache ({n} models)");
                }
                Err(e) => {
                    counters.error(format!("model-cache upsert: {e}"));
                    warning!("postvec: worker model-cache upsert failed: {e}");
                }
            }
        }
        Err(e) => {
            counters.error(format!("model refresh: {e}"));
            warning!("postvec: worker model refresh failed: {e}");
        }
    }
}

/// Upsert the singleton heartbeat row in the ambient transaction. One
/// `INSERT ... ON CONFLICT` against the row's fixed key — updates never touch
/// the key, so they stay HOT (no dead-tuple churn per poll tick from a
/// DELETE+INSERT). `stamp_refresh` records a completed model-cache refresh in
/// `models_refreshed_at` (the diff-aware cache upsert no longer proves
/// freshness through `models.last_seen`).
pub(crate) fn heartbeat_upsert(
    pid: i32,
    counters: &Counters,
    started_at: Option<&str>,
    stamp_refresh: bool,
) -> Result<(), pgrx::spi::Error> {
    Spi::run_with_args(
        "INSERT INTO postvec.worker_heartbeat
             (id, pid, last_beat, started_at, jobs_done, errors,
              jobs_embedded, jobs_nulled, jobs_retried, jobs_dead,
              rows_converted, rows_skipped, model_refreshes, last_error,
              documents_chunked, chunks_created, models_refreshed_at)
         VALUES (1, $1, now(), $2::timestamptz, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                 $13, $14, CASE WHEN $15 THEN now() END)
         ON CONFLICT (id) DO UPDATE SET
             pid = EXCLUDED.pid, last_beat = EXCLUDED.last_beat,
             started_at = EXCLUDED.started_at, jobs_done = EXCLUDED.jobs_done,
             errors = EXCLUDED.errors, jobs_embedded = EXCLUDED.jobs_embedded,
             jobs_nulled = EXCLUDED.jobs_nulled,
             jobs_retried = EXCLUDED.jobs_retried,
             jobs_dead = EXCLUDED.jobs_dead,
             rows_converted = EXCLUDED.rows_converted,
             rows_skipped = EXCLUDED.rows_skipped,
             model_refreshes = EXCLUDED.model_refreshes,
             last_error = EXCLUDED.last_error,
             documents_chunked = EXCLUDED.documents_chunked,
             chunks_created = EXCLUDED.chunks_created,
             models_refreshed_at = CASE WHEN $15 THEN now()
                 ELSE postvec.worker_heartbeat.models_refreshed_at END",
        &[
            pid.into(),
            started_at.into(),
            counters.jobs_done().into(),
            counters.errors.into(),
            counters.embedded.into(),
            counters.nulled.into(),
            counters.retried.into(),
            counters.dead.into(),
            counters.converted.into(),
            counters.skipped.into(),
            counters.refreshes.into(),
            counters.last_error.as_deref().into(),
            counters.documents_chunked.into(),
            counters.chunks_created.into(),
            stamp_refresh.into(),
        ],
    )
}

/// Change/interval gating state for the heartbeat. An unconditional write
/// per drain pass would produce two WAL-writing commits per second per
/// database on a completely idle cluster, so the machine could never go
/// quiet. Liveness monitoring keys off `postvec.heartbeat_interval_ms`.
#[derive(Default)]
struct HeartbeatState {
    last_write: Option<Instant>,
    last_written: Option<Counters>,
    /// A model-cache refresh completed since the last successful write; the
    /// next write stamps `models_refreshed_at`.
    stamp_refresh: bool,
}

fn write_heartbeat(counters: &Counters, started_at: Option<&str>, hb: &mut HeartbeatState) {
    let interval = Duration::from_millis(gucs::HEARTBEAT_INTERVAL_MS.get().max(1_000) as u64);
    let changed = hb.last_written.as_ref() != Some(counters);
    let due = hb.last_write.is_none_or(|at| at.elapsed() >= interval);
    if !changed && !due && !hb.stamp_refresh {
        return;
    }
    let pid = unsafe { pg_sys::MyProcPid };
    match try_transaction(|| heartbeat_upsert(pid, counters, started_at, hb.stamp_refresh)) {
        Ok(Ok(())) => {
            hb.last_write = Some(Instant::now());
            hb.last_written = Some(counters.clone());
            hb.stamp_refresh = false;
        }
        Ok(Err(e)) => warning!("postvec: worker heartbeat write failed: {e}"),
        // A version skew is reported by the gate, with the fix; repeating it
        // here as a heartbeat failure would only add noise.
        Err(e) if is_version_skew(&e) => {}
        Err(e) => warning!("postvec: worker heartbeat transaction failed: {e}"),
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use super::*;

    /// The respawn quarantine ladder, deterministically: consecutive
    /// instant deaths escalate 15 → 30 → 60 → 120 → 240 → 300 and stay
    /// capped; one healthy lifetime resets to the base. (Audit round 6: the
    /// scheduling-coupled evaluation oscillated 15/30 because the second
    /// observation landed exactly on the healthy threshold.)
    #[test]
    fn respawn_ladder_escalates_and_resets() {
        let mut ladder = RespawnLadder::new();
        let fast = Duration::from_millis(50);
        let delays: Vec<u64> = (0..7).map(|_| ladder.on_exit(fast).as_secs()).collect();
        assert_eq!(delays, vec![15, 30, 60, 120, 240, 300, 300]);

        // A healthy run resets the ladder...
        assert_eq!(
            ladder.on_exit(Duration::from_secs(3600)).as_secs(),
            15,
            "healthy lifetime resets to base"
        );
        // ...and escalation starts over from the base.
        assert_eq!(ladder.on_exit(fast).as_secs(), 15);
        assert_eq!(ladder.on_exit(fast).as_secs(), 30);
    }

    /// Refresh scheduling is pure Instant addition: no interval/delay
    /// combination can underflow (the earlier subtraction-based form
    /// panicked with the minimum 1 s interval), the per-database jitter
    /// stays within a quarter of its base at the minimum, default and
    /// maximum supported intervals, and the startup delay is stable and
    /// under 15 s.
    #[test]
    fn refresh_scheduling_is_underflow_free_across_supported_intervals() {
        for base_ms in [1_000u64, 60_000, 86_400_000] {
            let base = Duration::from_millis(base_ms);
            let j = refresh_jitter("some_database", base);
            assert!(j < base / 4 + Duration::from_millis(1), "jitter < base/4");
            // The full schedule computation: additions only.
            let _next = Instant::now() + startup_refresh_delay("some_database");
            let _following = Instant::now() + base + j;
        }
        let d = startup_refresh_delay("some_database");
        assert_eq!(d, startup_refresh_delay("some_database"), "stable");
        assert!(d < Duration::from_secs(15));
    }

    /// The launcher-mode inference gate probes the loopback listener with a
    /// plain TCP connect: accepting = engine ready, refused/invalid = closed.
    #[test]
    fn tcp_reachable_probes_a_live_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        assert!(tcp_reachable(&addr), "bound listener is reachable");
        drop(listener);
        // Port 0 parses as an address but can never identify a listening
        // TCP service, so the probe fails deterministically. (The obvious
        // alternatives are worse: re-probing the just-freed ephemeral port
        // is racy — another process can claim it between the drop and the
        // probe — and a low port like :1 is only closed by convention.)
        assert!(
            !tcp_reachable("127.0.0.1:0"),
            "port 0 can never be a listening service"
        );
        assert!(
            !tcp_reachable("not an address"),
            "unparseable = unreachable"
        );
    }

    /// A recurring condition is logged on first sighting, again when the
    /// state text changes, and otherwise only once per repeat interval —
    /// so a parked worker does not write one line per poll tick.
    #[test]
    fn recurring_reports_first_sighting_and_state_changes() {
        let mut r = Recurring::default();
        assert!(r.should_report("0.1.0"), "first sighting reports");
        assert!(!r.should_report("0.1.0"), "same state stays quiet");
        assert!(r.should_report("0.2.0"), "a changed state reports again");
        assert!(!r.should_report("0.2.0"));

        r.clear();
        assert!(r.should_report("0.2.0"), "after clearing, it reports again");
    }

    /// Every branch of the gate, over the catalog read it classifies.
    #[test]
    fn catalog_version_classification() {
        let this = env!("CARGO_PKG_VERSION").to_string();
        assert_eq!(
            classify_catalog_version(Ok(Some(this))),
            CatalogVersion::Matched
        );
        assert_eq!(
            classify_catalog_version(Ok(Some("0.0.1".into()))),
            CatalogVersion::Mismatched("0.0.1".into()),
            "an older SQL catalog parks the worker"
        );
        assert_eq!(
            classify_catalog_version(Ok(Some("99.0.0".into()))),
            CatalogVersion::Mismatched("99.0.0".into()),
            "a newer SQL catalog parks it too — the gate is equality, not ordering"
        );
        assert_eq!(classify_catalog_version(Ok(None)), CatalogVersion::Absent);
        assert_eq!(
            classify_catalog_version(Err("connection lost".into())),
            CatalogVersion::Unreadable("connection lost".into())
        );
    }

    /// …and the invariant the gate relies on: a freshly installed extension
    /// reports exactly the library's version, so the gate opens.
    /// (`catalog_version()` itself only runs inside a registered background
    /// worker, so the test reads the catalog directly.)
    #[pg_test]
    fn installed_catalog_version_equals_library_version() {
        let installed = Spi::get_one::<String>(CATALOG_VERSION_SQL).unwrap();
        assert_eq!(installed.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    /// The in-transaction guard, against a real catalog and a real advisory
    /// lock. This is the check that closes the window the outer gate cannot:
    /// it runs inside the same transaction as the work it protects.
    #[pg_test]
    fn schema_guard_opens_for_the_matching_version() {
        assert_eq!(
            guard_schema_version_against(env!("CARGO_PKG_VERSION")),
            Ok(())
        );
        // Taking the lock twice in one transaction is fine (it is shared, and
        // re-taking it is a no-op), which matters because every worker
        // transaction takes it.
        assert_eq!(
            guard_schema_version_against(env!("CARGO_PKG_VERSION")),
            Ok(())
        );
    }

    #[pg_test]
    fn schema_guard_refuses_a_mismatched_version() {
        let error = guard_schema_version_against("0.0.1").expect_err("must refuse");
        assert!(
            is_version_skew(&error),
            "the drain loop recognises this error by its prefix: {error}"
        );
        assert!(error.contains(env!("CARGO_PKG_VERSION")), "{error}");
        assert!(error.contains("0.0.1"), "{error}");
    }

    /// The outer gate must be able to *distinguish* its outcomes, not merely
    /// refuse. Its catalog read therefore goes through
    /// `try_catalog_transaction`, which does **not** run the schema guard: if
    /// it did, an absent extension and a mismatched one would both come back
    /// as the same opaque guard failure, and the worker would log "cannot read
    /// the installed version" where it should either stay silent (not
    /// installed yet) or print the `ALTER EXTENSION` fix.
    ///
    /// The transaction wrappers themselves only run inside a registered
    /// background worker, so this exercises the two halves the gate composes:
    /// the read it actually performs, and what feeding it a guard failure
    /// instead would do to the classification.
    #[pg_test]
    fn the_outer_gate_reads_the_catalog_without_the_guard() {
        let read = Spi::get_one::<String>(CATALOG_VERSION_SQL).map_err(|e| e.to_string());
        assert_eq!(
            classify_catalog_version(read),
            CatalogVersion::Matched,
            "an unguarded catalog read reports the real state"
        );

        // What the gate would see if it used the guarded wrapper: every
        // outcome collapses into one, and both diagnostics are lost.
        let through_the_guard = guard_schema_version_against("0.0.1").map(|()| None);
        assert!(matches!(
            classify_catalog_version(through_the_guard),
            CatalogVersion::Unreadable(_)
        ));
    }

    /// The lock the guard takes is the one an upgrade script is required to
    /// take exclusively. Proving it is a real, held lock is what makes that
    /// protocol meaningful.
    #[pg_test]
    fn the_schema_lock_is_actually_held() {
        guard_schema_version_against(env!("CARGO_PKG_VERSION")).unwrap();
        // Filter to this backend: the suite runs tests in parallel against
        // one shared database, and every worker-transaction test takes the
        // same *shared* lock — a global count is 2 whenever two overlap.
        let held = Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_locks
              WHERE locktype = 'advisory'
                AND objid = hashtext('postvec_schema')::oid
                AND mode = 'ShareLock'
                AND granted
                AND pid = pg_backend_pid()",
        )
        .unwrap();
        assert_eq!(held, Some(1), "the guard must hold the schema lock");
    }

    /// The heartbeat is a true single-row upsert: repeated writes update in
    /// place (no DELETE+INSERT churn) and the counters reflect the last write.
    #[pg_test]
    fn heartbeat_upserts_a_single_row() {
        let counters = Counters {
            embedded: 3,
            ..Default::default()
        };
        heartbeat_upsert(4242, &counters, Some("2026-07-04 00:00:00+00"), false).unwrap();

        let counters = Counters {
            embedded: 5,
            errors: 1,
            last_error: Some("boom".into()),
            ..Default::default()
        };
        heartbeat_upsert(4242, &counters, Some("2026-07-04 00:00:00+00"), false).unwrap();

        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.worker_heartbeat").unwrap(),
            Some(1)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT jobs_embedded FROM postvec.worker_heartbeat").unwrap(),
            Some(5)
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT last_error FROM postvec.worker_heartbeat")
                .unwrap()
                .as_deref(),
            Some("boom")
        );
    }
}
