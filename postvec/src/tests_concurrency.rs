//! Cross-session concurrency suite.
//!
//! The pg_test harness runs each test inside one backend's transaction,
//! which cannot exercise lock waiter behaviour. These tests therefore
//! drive real additional sessions with the `postgres` client (an optional
//! dependency activated only by the `pg_test_concurrency` feature; the
//! shipped cdylib never links it) and use bounded synchronization: a
//! waiter is observed through `pg_stat_activity`/`pg_locks` with a capped
//! poll, then released by a commit or rollback. Every secondary session
//! gets a `statement_timeout` backstop centrally (see `session()`), so a
//! locking regression fails the test instead of hanging the suite. Sleep
//! is never the correctness oracle.
//!
//! **Isolation.** Cross-session work needs *committed* fixtures, while the
//! ordinary pgrx suite runs its tests in parallel against one shared database
//! and asserts on global counts (`postvec.registry`, `postvec.jobs`) —
//! committing fixtures alongside it would corrupt those assertions. This
//! module therefore lives behind its own `pg_test_concurrency` feature and
//! runs as a separate `cargo pgrx test` invocation (see `ci.sh`); pgrx builds
//! a fresh cluster per invocation, so nothing else shares the database.
//!
//! Creating a scratch database per test instead is NOT viable here:
//! `DROP DATABASE ... WITH (FORCE)` waits on a `ProcSignalBarrier` that every
//! backend must absorb, and a test backend parked in a client-socket read
//! reaches no interrupt point — the two deadlock.

#[cfg(feature = "pg_test_concurrency")]
#[pgrx::pg_schema]
mod tests {
    use crate::registry::RegistryEntryDb as _;
    use pgrx::prelude::*;
    use std::time::Duration;

    /// Connection parameters, resolved ON THE MAIN THREAD (SPI is unusable
    /// from spawned threads); `Config` is Clone + Send and travels into them.
    fn conn_config(dbname: &str) -> postgres::Config {
        let port = Spi::get_one::<String>("SELECT current_setting('port')")
            .unwrap()
            .unwrap()
            .parse::<u16>()
            .unwrap();
        let user = Spi::get_one::<String>("SELECT current_user::text")
            .unwrap()
            .unwrap();
        let mut cfg = postgres::Config::new();
        cfg.host("127.0.0.1")
            .port(port)
            .dbname(dbname)
            .user(&user)
            .connect_timeout(Duration::from_secs(10));
        cfg
    }

    /// Every secondary session carries a `statement_timeout`, so a locking
    /// regression fails a test instead of hanging the suite.
    fn session(cfg: &postgres::Config) -> postgres::Client {
        let mut c = cfg
            .clone()
            .connect(postgres::NoTls)
            .expect("session connects to the test cluster");
        c.batch_execute("SET statement_timeout = '30s'").unwrap();
        c
    }

    /// The server's message. `postgres::Error`'s Display is only "db error".
    fn err_text(e: postgres::Error) -> String {
        e.as_db_error()
            .map(|d| d.message().to_string())
            .unwrap_or_else(|| e.to_string())
    }

    /// `start_worker()` runs a worker with no launcher and no preload: the
    /// heartbeat appears, a second call is a no-op, and a terminated worker
    /// exits cleanly instead of being respawned.
    #[pg_test]
    fn start_worker_runs_without_preload() {
        let db = Spi::get_one::<String>("SELECT current_database()::text")
            .unwrap()
            .unwrap();
        let mut c = session(&conn_config(&db));
        let workers = |c: &mut postgres::Client| -> i64 {
            c.query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() \
                 AND backend_type LIKE 'postvec worker%'",
                &[],
            )
            .unwrap()
            .get(0)
        };
        let wait =
            |c: &mut postgres::Client, what: &str, ok: &dyn Fn(&mut postgres::Client) -> bool| {
                let deadline = std::time::Instant::now() + Duration::from_secs(60);
                while !ok(c) {
                    assert!(std::time::Instant::now() < deadline, "timed out: {what}");
                    std::thread::sleep(Duration::from_millis(250));
                }
            };
        assert!(c
            .query_one("SELECT postvec.start_worker()", &[])
            .unwrap()
            .get::<_, bool>(0));
        wait(&mut c, "heartbeat", &|c| {
            c.query_one(
                "SELECT EXISTS (SELECT FROM postvec.worker_heartbeat \
                 WHERE last_beat > now() - interval '1 minute')",
                &[],
            )
            .unwrap()
            .get(0)
        });
        assert!(!c
            .query_one("SELECT postvec.start_worker()", &[])
            .unwrap()
            .get::<_, bool>(0));
        assert_eq!(workers(&mut c), 1);
        c.execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND backend_type LIKE 'postvec worker%'",
            &[],
        )
        .unwrap();
        wait(&mut c, "worker exit", &|c| workers(c) == 0);
        std::thread::sleep(Duration::from_secs(7));
        assert_eq!(workers(&mut c), 0, "a clean exit must not be respawned");
        assert!(c
            .query_one("SELECT postvec.start_worker()", &[])
            .unwrap()
            .get::<_, bool>(0));
        wait(&mut c, "second start", &|c| workers(c) == 1);
        c.execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = current_database() AND backend_type LIKE 'postvec worker%'",
            &[],
        )
        .unwrap();
        wait(&mut c, "cleanup", &|c| workers(c) == 0);
    }

    /// Committed fixtures live in the suite database (this module owns the
    /// invocation, so nothing else observes them). Each test uses its own
    /// table name and drops everything it created at the end.
    struct Fixture {
        table: String,
        cfg: postgres::Config,
    }

    impl Fixture {
        fn new(table: &str) -> Fixture {
            let db = Spi::get_one::<String>("SELECT current_database()::text")
                .unwrap()
                .unwrap();
            let cfg = conn_config(&db);
            let f = Fixture {
                table: table.to_string(),
                cfg,
            };
            // A previous aborted run must not leak into this one.
            f.cleanup();
            f
        }

        fn session(&self) -> postgres::Client {
            session(&self.cfg)
        }

        /// Remove every object and control row this fixture can own.
        fn cleanup(&self) {
            let table = &self.table;
            let mut c = session(&self.cfg);
            c.batch_execute(&format!(
                "DO $pv$
                 DECLARE r record;
                 BEGIN
                     FOR r IN SELECT id FROM postvec.registry
                               WHERE table_name = '{table}' LOOP
                         DELETE FROM postvec.jobs WHERE registry_id = r.id;
                         DELETE FROM postvec.jobs_dead WHERE registry_id = r.id;
                         DELETE FROM postvec.migrations WHERE registry_id = r.id;
                         DELETE FROM postvec.registry WHERE id = r.id;
                     END LOOP;
                 END $pv$;
                 DROP TABLE IF EXISTS {table} CASCADE;"
            ))
            .unwrap();
        }
    }

    /// Bounded wait (≤ 10 s) until `pid` blocks on a heavyweight lock.
    fn wait_until_blocked(pid: i32) {
        for _ in 0..200 {
            Spi::run("SELECT pg_stat_clear_snapshot()").unwrap();
            let blocked = Spi::get_one_with_args::<bool>(
                "SELECT coalesce(bool_or(wait_event_type = 'Lock'), false)
                   FROM pg_stat_activity WHERE pid = $1",
                &[pid.into()],
            )
            .unwrap()
            .unwrap_or(false);
            if blocked {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("backend {pid} never blocked on a lock");
    }

    fn backend_pid(c: &mut postgres::Client) -> i32 {
        c.query_one("SELECT pg_backend_pid()", &[]).unwrap().get(0)
    }

    /// Committed fixture: model row, table, synced entry. Returns its id.
    fn setup_entry(c: &mut postgres::Client, table: &str, extra_cols: &str, opts: &str) -> i64 {
        c.batch_execute(&format!(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{{}}'::jsonb) ON CONFLICT (name) DO NOTHING;
             CREATE TABLE {table} (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                   {extra_cols} body text);"
        ))
        .unwrap();
        c.query_one(
            &format!("SELECT postvec.enable('{table}','body','m', backfill => false{opts})"),
            &[],
        )
        .unwrap()
        .get(0)
    }

    #[pg_test]
    fn lexical_refresh_revalidates_after_disable_and_allows_writes() {
        let fx = Fixture::new("cc_lexical");
        let mut owner = fx.session();
        let rid = setup_entry(&mut owner, "cc_lexical", "", "");
        owner.batch_execute("BEGIN").unwrap();
        owner
            .query_one(
                "SELECT id FROM postvec.registry WHERE id=$1 FOR NO KEY UPDATE",
                &[&rid],
            )
            .unwrap();
        let mut refresh = fx.session();
        let pid = backend_pid(&mut refresh);
        let run = std::thread::spawn(move || {
            refresh
                .query_one("SELECT postvec._refresh_lexical_stats($1)", &[&rid])
                .map(|r| r.get::<_, Option<String>>(0))
                .map_err(err_text)
        });
        wait_until_blocked(pid);
        let mut writer = fx.session();
        writer
            .batch_execute("INSERT INTO cc_lexical(body) VALUES ('red red')")
            .unwrap();
        owner.batch_execute("COMMIT").unwrap();
        assert_eq!(run.join().unwrap().unwrap(), None);
        assert_eq!(
            owner
                .query_one(
                    "SELECT n FROM postvec.lexical_stats WHERE registry_id=$1",
                    &[&rid]
                )
                .unwrap()
                .get::<_, i64>(0),
            1
        );

        owner
            .batch_execute("BEGIN; SELECT postvec.disable('cc_lexical','body')")
            .unwrap();
        let mut refresh = fx.session();
        let pid = backend_pid(&mut refresh);
        let run = std::thread::spawn(move || {
            refresh
                .query_one("SELECT postvec._refresh_lexical_stats($1)", &[&rid])
                .map(|r| r.get::<_, Option<String>>(0))
                .map_err(err_text)
        });
        wait_until_blocked(pid);
        owner.batch_execute("COMMIT").unwrap();
        assert_eq!(run.join().unwrap().unwrap(), None);
        assert_eq!(
            owner
                .query_one(
                    "SELECT count(*) FROM postvec.lexical_stats WHERE registry_id=$1",
                    &[&rid]
                )
                .unwrap()
                .get::<_, i64>(0),
            0
        );
        drop(owner);
        drop(writer);
        fx.cleanup();
    }

    /// Two concurrent `retry_dead()` calls for the same explicit ids
    /// serialize on the `FOR UPDATE` row locks. Exactly one consumes them,
    /// and the waiter revalidates after the wait and reports the ids gone
    /// instead of claiming it moved rows the first call already took.
    #[pg_test]
    fn retry_dead_two_sessions_serialize() {
        let fx = Fixture::new("cc_redrive");
        let mut b1 = fx.session();
        let rid = setup_entry(&mut b1, "cc_redrive", "", "");
        let dead_id: i64 = b1
            .query_one(
                "INSERT INTO postvec.jobs_dead (job_id, registry_id, pk_value, attempts, last_error)
                 VALUES (0, $1, '1', 6, 'seeded') RETURNING dead_id",
                &[&rid],
            )
            .unwrap()
            .get(0);

        // Connect the waiter before B1 opens its transaction (backend startup
        // itself can wait on an open writing transaction).
        let mut b2 = fx.session();
        let b2_pid = backend_pid(&mut b2);

        // B1 consumes the ids and holds the locks open.
        b1.batch_execute("BEGIN").unwrap();
        let moved: i64 = b1
            .query_one(
                &format!(
                    "SELECT postvec.retry_dead('cc_redrive','body', ARRAY[{dead_id}]::bigint[])"
                ),
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(moved, 1, "the first caller consumes the dead row");

        let waiter = std::thread::spawn(move || {
            b2.query_one(
                &format!(
                    "SELECT postvec.retry_dead('cc_redrive','body', ARRAY[{dead_id}]::bigint[])"
                ),
                &[],
            )
            .map(|r| r.get::<_, i64>(0))
            .map_err(err_text)
        });
        wait_until_blocked(b2_pid);
        b1.batch_execute("COMMIT").unwrap();

        let err = waiter
            .join()
            .unwrap()
            .expect_err("the waiter must refuse after revalidating");
        assert!(
            err.contains("no longer exist"),
            "the waiter reports the consumed ids as gone: {err}"
        );
        // Scoped to this entry: the suite's tests run in parallel and other
        // fixtures have queue rows of their own.
        let counts = b1
            .query_one(
                "SELECT (SELECT count(*) FROM postvec.jobs WHERE registry_id = $1),
                        (SELECT count(*) FROM postvec.jobs_dead WHERE registry_id = $1)",
                &[&rid],
            )
            .unwrap();
        assert_eq!(
            (counts.get::<_, i64>(0), counts.get::<_, i64>(1)),
            (1, 0),
            "exactly one consumption happened for this entry"
        );
        drop(b1);
        fx.cleanup();
    }

    /// A source UPDATE waits behind an in-flight refresh's source-row
    /// `FOR SHARE` lock, and its trigger then removes the refresh's freshly
    /// committed output, so no chunks of the older row version survive the
    /// newer version's invalidation. Discriminating: with the `FOR SHARE`
    /// removed from the refresh read, the UPDATE does not block, its inline
    /// purge runs before the refresh commits its chunks and the stale
    /// chunk set survives, failing the final assertions.
    #[pg_test]
    fn p5_update_waits_behind_refresh_row_lock() {
        let fx = Fixture::new("cc_chunk");
        let mut b1 = fx.session();
        b1.batch_execute("DROP TABLE IF EXISTS cc_chunk_chunks CASCADE")
            .unwrap();
        b1.batch_execute(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING;
             CREATE TABLE cc_chunk (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                    body text);",
        )
        .unwrap();
        let rid: i64 = b1
            .query_one(
                "SELECT postvec.enable('cc_chunk','body','m', chunking => 'recursive',
                                       destination => 'cc_chunk_chunks',
                                       chunk_size => 64, chunk_overlap => 0,
                                       backfill => false)",
                &[],
            )
            .unwrap()
            .get(0);
        b1.batch_execute("INSERT INTO cc_chunk (body) VALUES ('version one of the document')")
            .unwrap();

        // Session 1 runs the REAL refresh inside an open transaction: the
        // source row is now locked FOR SHARE and the chunk rows are written
        // but uncommitted.
        b1.batch_execute("BEGIN").unwrap();
        let refreshed: bool = b1
            .query_one("SELECT postvec.__test_process_one_refresh()", &[])
            .unwrap()
            .get(0);
        assert!(refreshed, "the refresh claimed and processed the document");

        // Session 2's UPDATE must block on that row lock.
        let mut b2 = fx.session();
        let b2_pid = backend_pid(&mut b2);
        let writer = std::thread::spawn(move || {
            b2.batch_execute("UPDATE cc_chunk SET body = 'version two entirely'")
                .map_err(err_text)
        });
        wait_until_blocked(b2_pid);
        b1.batch_execute("COMMIT").unwrap();
        writer
            .join()
            .unwrap()
            .expect("the writer proceeds once the refresh commits");

        // The update's trigger ran AFTER the refresh committed, so it purged
        // the version-one chunks and left exactly one pending refresh for the
        // new version. Stale text must be gone.
        let row = b1
            .query_one(
                "SELECT (SELECT count(*) FROM cc_chunk_chunks),
                        (SELECT count(*) FROM postvec.jobs
                          WHERE registry_id = $1 AND op = 'refresh'
                            AND claimed_at IS NULL),
                        (SELECT count(*) FROM cc_chunk_chunks
                          WHERE chunk_text LIKE '%version one%')",
                &[&rid],
            )
            .unwrap();
        assert_eq!(
            (
                row.get::<_, i64>(0),
                row.get::<_, i64>(1),
                row.get::<_, i64>(2)
            ),
            (0, 1, 0),
            "the invalidation removed the refresh's output and re-enqueued"
        );

        // The pending refresh converges to the new version.
        let refreshed: bool = b1
            .query_one("SELECT postvec.__test_process_one_refresh()", &[])
            .unwrap()
            .get(0);
        assert!(refreshed);
        let stale: i64 = b1
            .query_one(
                "SELECT count(*) FROM cc_chunk_chunks WHERE chunk_text NOT LIKE '%version two%'",
                &[],
            )
            .unwrap()
            .get(0);
        assert_eq!(stale, 0, "only new-version chunks exist");

        b1.batch_execute("SELECT postvec.disable('cc_chunk','body', drop_destination => true)")
            .unwrap();
        drop(b1);
        fx.cleanup();
    }

    /// `set_format()`'s explicit table lock makes the full-refresh
    /// boundary exact even for an observed entry, which has no enqueue
    /// triggers whose replacement would otherwise conflict with DML. A
    /// concurrent writer waits, so no row can land between the finite
    /// all-row snapshot and the commit.
    #[pg_test]
    fn set_format_boundary_blocks_concurrent_writer() {
        let fx = Fixture::new("cc_fmt");
        let mut b1 = fx.session();
        b1.batch_execute(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING;
             CREATE TABLE cc_fmt (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                  title text, body text, embedding vector(3));
             INSERT INTO cc_fmt (title, body) VALUES ('t1','b1'), ('t2','b2');",
        )
        .unwrap();
        let rid: i64 = b1
            .query_one(
                "SELECT postvec.adopt('cc_fmt','body', vector_column => 'embedding',
                                      model => 'm', sync => false, backfill => 'none')",
                &[],
            )
            .unwrap()
            .get(0);

        let mut b2 = fx.session();
        let b2_pid = backend_pid(&mut b2);

        b1.batch_execute("BEGIN").unwrap();
        b1.batch_execute("SELECT postvec.set_format('cc_fmt','body', '$title $body')")
            .unwrap();

        let writer = std::thread::spawn(move || {
            b2.batch_execute("INSERT INTO cc_fmt (title, body) VALUES ('late','after')")
                .map_err(err_text)
        });
        wait_until_blocked(b2_pid);
        b1.batch_execute("COMMIT").unwrap();
        writer
            .join()
            .unwrap()
            .expect("the writer proceeds once the boundary commits");

        // The refresh enqueued exactly the two pre-boundary rows; the third
        // landed cleanly after the boundary (an observed entry has no
        // triggers, so it is not enqueued — it was never half-inside).
        let counts = b1
            .query_one(
                "SELECT (SELECT count(*) FROM postvec.jobs WHERE registry_id = $1),
                        (SELECT count(*) FROM cc_fmt)",
                &[&rid],
            )
            .unwrap();
        assert_eq!((counts.get::<_, i64>(0), counts.get::<_, i64>(1)), (2, 3));
        drop(b1);
        fx.cleanup();
    }

    /// DML racing an ordinary index build waits on the build's table lock
    /// and is then maintained by the completed index. No row is lost, and
    /// the racing row is served by an index scan afterwards.
    #[pg_test]
    fn create_index_blocks_racing_dml_and_maintains_it() {
        let fx = Fixture::new("cc_race");
        let mut b1 = fx.session();
        let rid = setup_entry(&mut b1, "cc_race", "", "");
        // Insert the pre-build row WITH its vector in one statement: an
        // insert-then-update leaves a broken HOT chain, which flags the fresh
        // index `indcheckxmin` and makes PostgreSQL ignore it for a while —
        // an artifact of the fixture, not of the build.
        b1.batch_execute(&format!(
            "INSERT INTO cc_race (body, body_semantic) VALUES ('first', '[1,0,0]');
                 DELETE FROM postvec.jobs WHERE registry_id = {rid};"
        ))
        .unwrap();

        let mut b2 = fx.session();
        let b2_pid = backend_pid(&mut b2);

        b1.batch_execute("BEGIN").unwrap();
        b1.batch_execute("SELECT postvec.create_vector_index('cc_race','body')")
            .unwrap();

        let writer = std::thread::spawn(move || {
            b2.batch_execute("INSERT INTO cc_race (body, body_semantic) VALUES ('racer','[0,0,1]')")
                .map_err(err_text)
        });
        wait_until_blocked(b2_pid);
        b1.batch_execute("COMMIT").unwrap();
        writer
            .join()
            .unwrap()
            .expect("the racing insert proceeds once the build commits");

        b1.batch_execute("BEGIN; SET LOCAL enable_seqscan = off")
            .unwrap();
        let nearest: String = b1
            .query_one(
                "SELECT id::text FROM cc_race WHERE body_semantic IS NOT NULL
                  ORDER BY body_semantic <=> '[0,0,1]'::vector LIMIT 1",
                &[],
            )
            .unwrap()
            .get(0);
        let plan: String = b1
            .query(
                "EXPLAIN SELECT id FROM cc_race WHERE body_semantic IS NOT NULL
                  ORDER BY body_semantic <=> '[0,0,1]'::vector LIMIT 1",
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect::<Vec<_>>()
            .join("\n");
        b1.batch_execute("COMMIT").unwrap();
        assert_eq!(nearest, "2", "the row inserted during the build is found");
        assert!(
            plan.contains(&format!("postvec_vec_{rid}")),
            "and it is served BY the completed index: {plan}"
        );
        drop(b1);
        fx.cleanup();
    }

    /// Lock order, observed from the inside: while the build is parked
    /// waiting for the registry row, it must ALREADY hold the managed
    /// table's lock — proof that it takes the table first, the same order
    /// every lifecycle verb uses (see
    /// [`auto_build_uses_the_lifecycle_lock_order`] for why the reverse
    /// deadlocks). Under the inverted order the parked build holds no table
    /// lock at all, which fails this assertion.
    ///
    /// The holder here takes only the registry row: a real, safe state (a
    /// second builder that already has the table, or `record_build_failure()`,
    /// which never touches the table).
    #[pg_test]
    fn auto_build_takes_the_table_before_the_registry_row() {
        let fx = Fixture::new("cc_lock");
        let mut b1 = fx.session();
        let rid = setup_entry(&mut b1, "cc_lock", "", ", index_mode => 'auto'");

        let mut builder = fx.session();
        let builder_pid = backend_pid(&mut builder);

        b1.batch_execute("BEGIN").unwrap();
        b1.query_one(
            "SELECT id FROM postvec.registry WHERE id = $1 FOR UPDATE",
            &[&rid],
        )
        .unwrap();

        let build = std::thread::spawn(move || {
            builder
                .batch_execute(&format!("SELECT postvec.__test_build_candidate({rid})"))
                .map_err(err_text)
        });
        wait_until_blocked(builder_pid);

        let table_lock: Option<String> = b1
            .query_one(
                "SELECT max(mode) FROM pg_locks
                  WHERE pid = $1 AND granted AND relation = 'cc_lock'::regclass",
                &[&builder_pid],
            )
            .unwrap()
            .get(0);
        assert_eq!(
            table_lock.as_deref(),
            Some("ShareLock"),
            "the parked build must already hold the table's SHARE lock: it has to \
             take the table BEFORE the registry row, matching every lifecycle verb"
        );

        b1.batch_execute("ROLLBACK").unwrap();
        build
            .join()
            .unwrap()
            .expect("the build completes once the row lock releases");
        let built: bool = b1
            .query_one(
                &format!("SELECT to_regclass('postvec_vec_{rid}') IS NOT NULL"),
                &[],
            )
            .unwrap()
            .get(0);
        assert!(built, "and then builds normally");
        drop(b1);
        fx.cleanup();
    }

    /// Lock order against a real lifecycle shape: every lifecycle verb
    /// takes the managed table before updating the registry row
    /// (`set_format()` locks SHARE ROW EXCLUSIVE first; `disable()` runs its
    /// trigger teardown first; `migrate()` adds the shadow column first).
    /// The builder must use that same order, or the two deadlock:
    ///
    /// ```text
    /// builder:   registry row → waits for table       (WRONG order)
    /// lifecycle: table        → waits for registry row
    /// ```
    ///
    /// Here the holder takes the table and, once the builder is parked,
    /// updates the registry row. With the correct order the builder is
    /// waiting on the TABLE and holds no row lock, so that update returns
    /// immediately. With the inverted order the builder is holding the row,
    /// the update blocks, and PostgreSQL breaks the cycle by aborting one of
    /// them — which fails this test either way (the update errors, or the
    /// builder does). Verified discriminating by inverting the order.
    #[pg_test]
    fn auto_build_uses_the_lifecycle_lock_order() {
        let fx = Fixture::new("cc_order");
        let mut b1 = fx.session();
        let rid = setup_entry(&mut b1, "cc_order", "", ", index_mode => 'auto'");

        let mut builder = fx.session();
        let builder_pid = backend_pid(&mut builder);

        // A lifecycle verb's shape: the managed table first.
        b1.batch_execute("BEGIN").unwrap();
        b1.batch_execute("LOCK TABLE cc_order IN ACCESS EXCLUSIVE MODE")
            .unwrap();

        let build = std::thread::spawn(move || {
            builder
                .batch_execute(&format!("SELECT postvec.__test_build_candidate({rid})"))
                .map_err(err_text)
        });
        wait_until_blocked(builder_pid);

        // The registry update every such verb makes next. It must NOT be
        // blocked by the parked builder: that is the whole point of the
        // shared lock order.
        b1.batch_execute(&format!(
            "UPDATE postvec.registry SET index_error = NULL WHERE id = {rid}"
        ))
        .expect("the lifecycle verb's registry update must not block on the builder");
        b1.batch_execute("COMMIT").unwrap();

        build
            .join()
            .unwrap()
            .expect("the build completes once the lifecycle verb releases the table");
        drop(b1);
        fx.cleanup();
    }

    /// Error parking vs a concurrent manual repair. `record_build_failure()`
    /// takes the registry row before rechecking readiness, so a winning
    /// `create_vector_index()` — which clears `index_error` under that same
    /// row lock — cannot slip between the check and the update and leave a
    /// stale parked error behind.
    ///
    /// Without the explicit `FOR UPDATE` the failure path still *waits*, just
    /// too late: it reads readiness first (the winner's index is not yet
    /// visible), then blocks on the row lock at its own UPDATE, and once the
    /// winner commits it writes the now-stale error anyway. So the final
    /// assertion, not `wait_until_blocked`, is what discriminates here.
    #[pg_test]
    fn error_parking_serializes_with_a_manual_winner() {
        let fx = Fixture::new("cc_win");
        let mut b1 = fx.session();
        let rid = setup_entry(&mut b1, "cc_win", "", ", index_mode => 'auto'");

        pgrx::log!("ccdrop: record failure");
        let mut failer = fx.session();
        let failer_pid = backend_pid(&mut failer);

        // The manual winner, mid-flight: index built, registry row held.
        b1.batch_execute("BEGIN").unwrap();
        b1.batch_execute("SELECT postvec.create_vector_index('cc_win','body')")
            .unwrap();

        let park = std::thread::spawn(move || {
            failer
                .batch_execute(&format!(
                    "SELECT postvec.__test_record_build_failure({rid}, 'boom')"
                ))
                .map_err(err_text)
        });
        wait_until_blocked(failer_pid);
        b1.batch_execute("COMMIT").unwrap();
        park.join()
            .unwrap()
            .expect("the failure path completes once the winner commits");

        let parked: Option<String> = b1
            .query_one(
                "SELECT index_error FROM postvec.registry WHERE id = $1",
                &[&rid],
            )
            .unwrap()
            .get(0);
        assert_eq!(
            parked, None,
            "the concurrent manual winner suppresses the error: readiness is \
             rechecked under the row lock the winner just released"
        );
        drop(b1);
        fx.cleanup();
    }

    /// Identity reconciliation against an uncommitted DROP: the window a
    /// single-session test cannot reach.
    ///
    /// While the DROP is uncommitted the build blocks on the table lock and
    /// dies on `lock_timeout`, and the fresh failure transaction still reads
    /// the pre-DROP catalog: identity looks healthy, so the entry is parked.
    /// The DROP then commits, and a parked entry is outside
    /// `eligible_entries()` — so only reconciliation that ignores
    /// `index_error` can still quarantine it. Without that decoupling the
    /// entry stays `active` with a dead table forever, which is what this
    /// test asserts against.
    #[pg_test]
    fn uncommitted_drop_during_build_is_reconciled_after_commit() {
        let fx = Fixture::new("cc_drop");
        let mut b1 = fx.session();
        let rid = setup_entry(&mut b1, "cc_drop", "", ", index_mode => 'auto'");

        let mut builder = fx.session();
        // The worker's own bound (postvec.worker_lock_timeout_ms), shortened.
        builder.batch_execute("SET lock_timeout = '2s'").unwrap();
        let builder_pid = backend_pid(&mut builder);

        // Session A: DROP, held open.
        b1.batch_execute("BEGIN").unwrap();
        b1.batch_execute("DROP TABLE cc_drop").unwrap();

        // The build waits on A's ACCESS EXCLUSIVE lock and times out.
        let build = std::thread::spawn(move || {
            builder
                .batch_execute(&format!("SELECT postvec.__test_build_candidate({rid})"))
                .map_err(err_text)
        });
        wait_until_blocked(builder_pid);
        pgrx::log!("ccdrop: build joined next");
        let build_err = build
            .join()
            .unwrap()
            .expect_err("the build must die on lock_timeout");
        assert!(
            build_err.contains("lock timeout") || build_err.contains("canceling"),
            "the build failed on the table lock: {build_err}"
        );

        // The failure recorder runs while the DROP is STILL uncommitted, so
        // the catalog looks healthy to it and it parks the entry.
        let mut failer = fx.session();
        failer
            .batch_execute(&format!(
                "SELECT postvec.__test_record_build_failure({rid}, 'lock timeout')"
            ))
            .unwrap();
        let parked: Option<String> = failer
            .query_one(
                "SELECT index_error FROM postvec.registry WHERE id = $1",
                &[&rid],
            )
            .unwrap()
            .get(0);
        assert!(
            parked.is_some(),
            "the uncommitted DROP is invisible, so the entry is parked — this is \
             exactly the state reconciliation has to recover from"
        );

        pgrx::log!("ccdrop: committing drop");
        // Now the DROP commits: the entry is parked AND its table is gone.
        b1.batch_execute("COMMIT").unwrap();
        pgrx::log!("ccdrop: drop committed; pick");

        // The next scan must still reconcile it, despite the parked error.
        // This runs the PRODUCTION scan (`pick_candidate()`), not the
        // reconciliation helper directly: the point is that the scan itself
        // reconciles before filtering on eligibility, so dropping that call
        // must fail this test. Running it in a session also commits the
        // quarantine — doing it in this backend would leave an uncommitted
        // registry-row lock that blocks the fixture cleanup below.
        failer
            .batch_execute("SELECT postvec.__test_pick_candidate()")
            .unwrap();
        pgrx::log!("ccdrop: picked");
        let state: String = failer
            .query_one("SELECT state FROM postvec.registry WHERE id = $1", &[&rid])
            .unwrap()
            .get(0);
        assert_eq!(
            state, "disabled",
            "a parked entry whose table vanished must still be quarantined"
        );
        drop(b1);
        drop(failer);
        pgrx::log!("ccdrop: cleanup");
        fx.cleanup();
    }

    /// Claim/read must pin the source definition before claiming a job row.
    /// Otherwise it inverts disable/quarantine's table -> job order and a
    /// concurrent lifecycle verb can deadlock with the worker.
    #[pg_test]
    fn claim_waits_for_relation_before_locking_job_row() {
        let fx = Fixture::new("cc_claim_order");
        let mut ddl = fx.session();
        let rid = setup_entry(&mut ddl, "cc_claim_order", "", "");
        ddl.batch_execute("INSERT INTO cc_claim_order(body) VALUES ('queued')")
            .unwrap();

        ddl.batch_execute("BEGIN; LOCK TABLE cc_claim_order IN ACCESS EXCLUSIVE MODE")
            .unwrap();

        let mut worker = fx.session();
        let worker_pid = backend_pid(&mut worker);
        let run = std::thread::spawn(move || {
            worker
                .query_one("SELECT postvec.__test_claim_and_read()", &[])
                .map(|r| r.get::<_, i64>(0))
                .map_err(err_text)
        });
        wait_until_blocked(worker_pid);

        // The worker is waiting on the relation, not holding the queue row.
        // NOWAIT makes this a deterministic discriminator against the old
        // claim-first implementation.
        let mut observer = fx.session();
        observer.batch_execute("BEGIN").unwrap();
        observer
            .query_one(
                "SELECT id FROM postvec.jobs
                  WHERE registry_id = $1 AND op = 'embed'
                  FOR UPDATE NOWAIT",
                &[&rid],
            )
            .expect("the queued job remains unlocked while relation pinning waits");
        observer.batch_execute("COMMIT").unwrap();

        ddl.batch_execute("COMMIT").unwrap();
        assert_eq!(
            run.join()
                .unwrap()
                .expect("claim/read resumes after the relation lock"),
            1
        );
        drop(ddl);
        fx.cleanup();
    }

    /// Round 8: `enable()` takes its ACCESS SHARE relation lock BEFORE the
    /// byte-boundability classification, so a concurrent ALTER COLUMN TYPE
    /// cannot commit between classification and the trigger/column DDL.
    /// Discriminating: without the early lock, `enable()` classifies the
    /// old committed type (text — fine), then blocks at its own DDL, and
    /// after the ALTER commits it finishes successfully against a jsonb
    /// source; with the lock it blocks FIRST and the post-ALTER
    /// classification refuses.
    #[pg_test]
    fn enable_blocks_behind_alter_type_and_reclassifies() {
        let fx = Fixture::new("cc_altertype");
        let mut b1 = fx.session();
        b1.batch_execute(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING;
             CREATE TABLE cc_altertype (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                        body text);",
        )
        .unwrap();

        let mut b2 = fx.session();
        let b2_pid = backend_pid(&mut b2);

        // B1 holds an uncommitted ALTER COLUMN TYPE (ACCESS EXCLUSIVE).
        b1.batch_execute(
            "BEGIN;
             ALTER TABLE cc_altertype ALTER COLUMN body TYPE jsonb USING to_jsonb(body);",
        )
        .unwrap();

        // B2's enable() must block at its early relation lock...
        let waiter = std::thread::spawn(move || {
            b2.query_one("SELECT postvec.enable('cc_altertype','body','m')", &[])
                .map(|r| r.get::<_, i64>(0))
                .map_err(err_text)
        });
        wait_until_blocked(b2_pid);
        b1.batch_execute("COMMIT").unwrap();

        // ...and, classifying only after the ALTER committed, refuse the
        // now-jsonb source.
        let err = waiter
            .join()
            .unwrap()
            .expect_err("enable must refuse the type the ALTER committed");
        assert!(
            err.contains("text-like"),
            "the refusal is the boundability classification: {err}"
        );
        drop(b1);
        fx.cleanup();
    }

    /// Round 8, the worker half: dependency validation takes (and RETAINS)
    /// the relation's ACCESS SHARE lock before re-classifying rendered
    /// columns, so it serializes against a concurrent ALTER COLUMN TYPE —
    /// validation that starts while an ALTER is in flight blocks, then sees
    /// the committed jsonb type and reports the entry quarantinable.
    /// Discriminating: without the lock the validation does not block, the
    /// uncommitted ALTER is invisible to its catalog reads, and it reports
    /// the entry healthy — failing the assertion below.
    #[pg_test]
    fn worker_validation_serializes_with_alter_type() {
        let fx = Fixture::new("cc_wval");
        let mut b1 = fx.session();
        setup_entry(&mut b1, "cc_wval", "", "");

        // B1 holds the uncommitted ALTER (ACCESS EXCLUSIVE) on the source.
        b1.batch_execute(
            "BEGIN;
             ALTER TABLE cc_wval ALTER COLUMN body TYPE jsonb USING to_jsonb(body);",
        )
        .unwrap();

        // The validating "worker" is THIS backend. The watcher thread owns
        // B1 and commits it once it observes this backend blocked on the
        // relation lock — and after a bounded wait regardless, so a locking
        // regression fails the assertion instead of hanging the suite.
        let my_pid = Spi::get_one::<i32>("SELECT pg_backend_pid()")
            .unwrap()
            .unwrap();
        let cfg = fx.cfg.clone();
        let watcher = std::thread::spawn(move || {
            let mut w = session(&cfg);
            for _ in 0..200 {
                let blocked: bool = w
                    .query_one(
                        "SELECT coalesce(bool_or(wait_event_type = 'Lock'), false)
                           FROM pg_stat_activity WHERE pid = $1",
                        &[&my_pid],
                    )
                    .map(|r| r.get(0))
                    .unwrap_or(false);
                if blocked {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            b1.batch_execute("COMMIT").unwrap();
        });

        let entry = crate::registry::RegistryEntry::load_active("public", "cc_wval", "body")
            .expect("the committed entry is visible");
        let reason = entry.missing_dependency_locked(&[]);
        watcher.join().unwrap();
        let reason = reason.expect("validation must observe the committed ALTER and quarantine");
        assert!(
            reason.contains("byte-boundable"),
            "the reason is the re-classification: {reason}"
        );
        // The retained validation lock — the very behaviour this test proves
        // — blocks any cross-session DROP of cc_wval until this backend's
        // transaction ends, and the pg_test transaction only ever rolls
        // back. So: clean the committed control rows from a session (the
        // part other tests' scoped assertions could see); the leaked empty
        // table is inert and Fixture::new's upfront cleanup removes it on
        // the next run.
        let mut c = fx.session();
        c.batch_execute(
            "DELETE FROM postvec.jobs WHERE registry_id IN
                 (SELECT id FROM postvec.registry WHERE table_name = 'cc_wval');
             DELETE FROM postvec.registry WHERE table_name = 'cc_wval';",
        )
        .unwrap();
    }

    /// Recursive worker validation pins the managed destination as well as
    /// the source. Without the destination lock this call observes the old
    /// committed RLS state while the ALTER is in flight and incorrectly
    /// reports the entry healthy; with the lock it waits and validates the
    /// committed broken state before any generated chunk SQL can run.
    #[pg_test]
    fn recursive_worker_validation_serializes_with_destination_ddl() {
        let fx = Fixture::new("cc_dval");
        let mut b1 = fx.session();
        b1.batch_execute("DROP TABLE IF EXISTS cc_dval_chunks CASCADE")
            .unwrap();
        let rid = setup_entry(
            &mut b1,
            "cc_dval",
            "",
            ", chunking => 'recursive', destination => 'cc_dval_chunks', \
             chunk_size => 64, chunk_overlap => 0",
        );

        b1.batch_execute("BEGIN; ALTER TABLE cc_dval_chunks DISABLE ROW LEVEL SECURITY")
            .unwrap();

        let my_pid = Spi::get_one::<i32>("SELECT pg_backend_pid()")
            .unwrap()
            .unwrap();
        let cfg = fx.cfg.clone();
        let watcher = std::thread::spawn(move || {
            let mut w = session(&cfg);
            for _ in 0..200 {
                let blocked: bool = w
                    .query_one(
                        "SELECT coalesce(bool_or(wait_event_type = 'Lock'), false)
                           FROM pg_stat_activity WHERE pid = $1",
                        &[&my_pid],
                    )
                    .map(|r| r.get(0))
                    .unwrap_or(false);
                if blocked {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            b1.batch_execute("COMMIT").unwrap();
        });

        let entry = crate::registry::RegistryEntry::load(rid).unwrap();
        let reason = entry.missing_dependency_locked(&[]);
        watcher.join().unwrap();
        let reason = reason.expect("validation must observe the committed destination DDL");
        assert!(reason.contains("row security"), "got: {reason}");

        // As in the source-lock test above, the pg_test transaction retains
        // its relation locks until harness rollback. Remove globally visible
        // control rows now; the next run's upfront cleanup removes the inert
        // relations once this transaction has ended.
        let mut c = fx.session();
        c.batch_execute(
            "DELETE FROM postvec.jobs WHERE registry_id IN
                 (SELECT id FROM postvec.registry WHERE table_name = 'cc_dval');
             DELETE FROM postvec.registry WHERE table_name = 'cc_dval';",
        )
        .unwrap();
    }
}
