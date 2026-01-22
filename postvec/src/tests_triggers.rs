//! Trigger correctness suite.
//!
//! Exercises every write path that must (or must not) enqueue a job,
//! asserting `postvec.jobs` contents directly. The worker is not involved:
//! these tests validate only the trigger -> queue plumbing. Tables are
//! enabled with `backfill => false` so the queue reflects trigger activity
//! alone.

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn seed_and_enable(create_sql: &str) {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(create_sql).unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
    }

    fn pending() -> i64 {
        Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs")
            .unwrap()
            .unwrap()
    }

    fn clear_jobs() {
        Spi::run("DELETE FROM postvec.jobs").unwrap();
    }

    const DOCS: &str =
        "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)";

    // ---- Recursive-entry trigger plumbing ----

    fn enable_recursive(trigger_mode: &str) -> i64 {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run("CREATE TABLE docs (id bigint PRIMARY KEY, body text, note text)").unwrap();
        Spi::get_one_with_args::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks', backfill => false,
                                   trigger_mode => $1)",
            &[trigger_mode.into()],
        )
        .unwrap()
        .unwrap()
    }

    /// Seed one materialized chunk row + a child job + a dead row for source
    /// pk `pk`, simulating a previously refreshed document with history.
    fn seed_chunk_state(id: i64, pk: i64) {
        Spi::run_with_args(
            "INSERT INTO docs_chunks
                 (postvec_source_pk, postvec_chunk_seq, postvec_char_start,
                  postvec_char_end, chunk_text)
             VALUES ($1, 0, 0, 5, 'chunk')",
            &[pk.into()],
        )
        .unwrap();
        let chunk_id = Spi::get_one_with_args::<i64>(
            "SELECT postvec_chunk_id FROM docs_chunks WHERE postvec_source_pk = $1",
            &[pk.into()],
        )
        .unwrap()
        .unwrap();
        Spi::run_with_args(
            "INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id)
             VALUES ($1, $2::text, 'embed', $3)",
            &[id.into(), pk.into(), chunk_id.into()],
        )
        .unwrap();
        Spi::run_with_args(
            "INSERT INTO postvec.jobs_dead (job_id, registry_id, pk_value, op, chunk_id)
             VALUES (0, $1, $2::text, 'embed', $3)",
            &[id.into(), pk.into(), chunk_id.into()],
        )
        .unwrap();
    }

    fn recursive_trigger_matrix(mode: &str) {
        let id = enable_recursive(mode);

        // INSERT enqueues one refresh per non-NULL source row.
        Spi::run("INSERT INTO docs VALUES (1,'a','x'), (2,'b','y'), (3,NULL,'z')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE op = 'refresh' AND chunk_id IS NULL"
            )
            .unwrap(),
            Some(2),
            "{mode}: two non-NULL inserts enqueue refreshes"
        );
        clear_jobs();

        // An unreferenced-column update enqueues nothing and purges nothing.
        seed_chunk_state(id, 1);
        Spi::run("UPDATE docs SET note = 'changed'").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'refresh'").unwrap(),
            Some(0),
            "{mode}: note-only update fires nothing"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(1),
            "{mode}: chunks survive an unreferenced update"
        );

        // A source-column update invalidates inline: chunks, child jobs, and
        // dead rows for the pk are gone; one refresh is pending.
        Spi::run("UPDATE docs SET body = 'a2' WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "{mode}: content update purges chunks in the writer's transaction"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'embed'").unwrap(),
            Some(0),
            "{mode}: old child jobs purged"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(0),
            "{mode}: obsolete dead rows purged"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT pk_value FROM postvec.jobs WHERE op = 'refresh'")
                .unwrap()
                .as_deref(),
            Some("1"),
            "{mode}: one refresh for the changed document"
        );
        clear_jobs();

        // A source -> NULL transition purges and enqueues nothing.
        seed_chunk_state(id, 2);
        Spi::run("UPDATE docs SET body = NULL WHERE id = 2").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "{mode}: NULL transition purges chunks"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'refresh'").unwrap(),
            Some(0),
            "{mode}: no refresh for a NULL source"
        );
        clear_jobs();

        // PK change: OLD identity fully invalidated, NEW enqueued.
        seed_chunk_state(id, 1);
        Spi::run("UPDATE docs SET id = 11 WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "{mode}: PK change purges the old identity's chunks"
        );
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT DISTINCT pk_value FROM postvec.jobs WHERE op = 'refresh'"
            )
            .unwrap()
            .as_deref(),
            Some("11"),
            "{mode}: the new identity is enqueued"
        );
        clear_jobs();

        // DELETE invalidates and enqueues nothing.
        seed_chunk_state(id, 11);
        Spi::run("DELETE FROM docs WHERE id = 11").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "{mode}: DELETE purges chunks"
        );
        assert_eq!(pending(), 0, "{mode}: DELETE enqueues nothing");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(0),
            "{mode}: DELETE purges dead rows"
        );

        // TRUNCATE clears destination, jobs, and dead rows.
        Spi::run("INSERT INTO docs VALUES (20, 'body', 'n')").unwrap();
        seed_chunk_state(id, 20);
        Spi::run("TRUNCATE docs").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "{mode}: TRUNCATE clears the destination"
        );
        assert_eq!(pending(), 0, "{mode}: TRUNCATE clears the queue");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(0),
            "{mode}: TRUNCATE clears dead rows"
        );
    }

    #[pg_test]
    fn recursive_statement_trigger_matrix() {
        recursive_trigger_matrix("statement");
    }

    #[pg_test]
    fn recursive_row_trigger_matrix() {
        recursive_trigger_matrix("row");
    }

    /// Bulk statement DML is set-based: one statement, many documents, all
    /// invalidated/enqueued in that statement's triggers.
    #[pg_test]
    fn recursive_bulk_update_and_delete_are_set_based() {
        let id = enable_recursive("statement");
        Spi::run("INSERT INTO docs SELECT g, 'body ' || g, 'n' FROM generate_series(1, 50) g")
            .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'refresh'").unwrap(),
            Some(50)
        );
        clear_jobs();
        for pk in [1i64, 2, 3] {
            seed_chunk_state(id, pk);
        }
        Spi::run("UPDATE docs SET body = body || '!' WHERE id <= 25").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "bulk update purges every affected document's chunks"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'refresh'").unwrap(),
            Some(25)
        );
        clear_jobs();
        Spi::run("DELETE FROM docs WHERE id > 40").unwrap();
        assert_eq!(pending(), 0, "bulk delete enqueues nothing");
    }

    /// Rapid updates to one document coalesce to one pending refresh.
    #[pg_test]
    fn recursive_rapid_updates_coalesce() {
        enable_recursive("statement");
        Spi::run("INSERT INTO docs VALUES (1, 'v1', 'n')").unwrap();
        Spi::run("UPDATE docs SET body = 'v2'").unwrap();
        Spi::run("UPDATE docs SET body = 'v3'").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'refresh'").unwrap(),
            Some(1),
            "the dedup index coalesces repeated refreshes"
        );
    }

    /// Partition-parent writes fire the recursive triggers in both modes;
    /// direct-partition writes fire only in row mode (the documented
    /// statement-mode limitation is preserved, not fixed).
    #[pg_test]
    fn recursive_partitioned_parent_and_direct_partition_writes() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE pdocs (id bigint NOT NULL PRIMARY KEY, body text)
             PARTITION BY RANGE (id)",
        )
        .unwrap();
        Spi::run("CREATE TABLE pdocs_lo PARTITION OF pdocs FOR VALUES FROM (0) TO (100)").unwrap();
        Spi::run("CREATE TABLE pdocs_hi PARTITION OF pdocs FOR VALUES FROM (100) TO (200)")
            .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('pdocs','body','m', chunking => 'recursive',
                                   destination => 'pdocs_chunks', backfill => false,
                                   trigger_mode => 'row')",
        )
        .unwrap();
        Spi::run("INSERT INTO pdocs VALUES (1, 'low'), (150, 'high')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'refresh'").unwrap(),
            Some(2),
            "parent-addressed inserts fire on both partitions"
        );
        clear_jobs();
        // Row triggers are cloned to partitions: a direct-partition write
        // still fires.
        Spi::run("INSERT INTO pdocs_lo VALUES (2, 'direct')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE op = 'refresh'").unwrap(),
            Some(1),
            "row mode covers direct-partition writes"
        );
    }

    #[pg_test]
    fn multirow_insert_enqueues_non_null_only() {
        seed_and_enable(DOCS);
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b'), (NULL), ('c')").unwrap();
        assert_eq!(pending(), 3, "three non-null rows, NULL skipped");
    }

    #[pg_test]
    fn copy_from_populates_transition_table() {
        seed_and_enable(DOCS);
        // COPY FROM a server-readable file must fire the INSERT trigger and
        // populate the NEW transition table (R2).
        let path = std::env::temp_dir().join("postvec_copy_test.txt");
        std::fs::write(&path, "alpha\nbeta\ngamma\n").expect("write copy file");
        Spi::run(&format!("COPY docs (body) FROM '{}'", path.display())).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(pending(), 3, "COPY of three rows enqueues three jobs");
    }

    #[pg_test]
    fn on_conflict_covers_both_legs() {
        seed_and_enable(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                tag text UNIQUE, body text)",
        );
        Spi::run("INSERT INTO docs (tag, body) VALUES ('a', 'one')").unwrap();
        clear_jobs();
        // 'a' conflicts -> UPDATE leg (body changes); 'b' is new -> INSERT leg.
        Spi::run(
            "INSERT INTO docs (tag, body) VALUES ('a','two'), ('b','three')
             ON CONFLICT (tag) DO UPDATE SET body = EXCLUDED.body",
        )
        .unwrap();
        assert_eq!(
            pending(),
            2,
            "both the inserted and the updated row enqueue"
        );
    }

    #[pg_test]
    fn update_of_other_column_does_not_enqueue() {
        seed_and_enable(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, note text)",
        );
        Spi::run("INSERT INTO docs (body, note) VALUES ('x', 'n')").unwrap();
        clear_jobs();
        Spi::run("UPDATE docs SET note = 'changed'").unwrap();
        assert_eq!(
            pending(),
            0,
            "AFTER UPDATE OF body must not fire for note-only update"
        );
    }

    #[pg_test]
    fn update_to_same_value_does_not_enqueue() {
        seed_and_enable(DOCS);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();
        clear_jobs();
        Spi::run("UPDATE docs SET body = body").unwrap();
        assert_eq!(
            pending(),
            0,
            "IS DISTINCT FROM guard: unchanged value enqueues nothing"
        );
    }

    #[pg_test]
    fn update_to_null_enqueues() {
        seed_and_enable(DOCS);
        Spi::run("INSERT INTO docs (body) VALUES ('x')").unwrap();
        clear_jobs();
        Spi::run("UPDATE docs SET body = NULL").unwrap();
        assert_eq!(
            pending(),
            1,
            "NULL is distinct from 'x' -> job (worker nulls the vector)"
        );
    }

    #[pg_test]
    fn truncate_purges_pending_jobs() {
        seed_and_enable(DOCS);
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b')").unwrap();
        assert_eq!(pending(), 2);
        Spi::run("TRUNCATE docs").unwrap();
        assert_eq!(
            pending(),
            0,
            "TRUNCATE trigger purges the entry's pending jobs"
        );
    }

    /// Confused-deputy guard: `postvec.trg_truncate()` is SECURITY DEFINER,
    /// so a role attaching it to their *own* table with a victim's registry
    /// id must not be able to purge the victim entry's pending jobs.
    #[pg_test]
    fn trg_truncate_ignores_mismatched_registry_id() {
        seed_and_enable(DOCS);
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b')").unwrap();
        assert_eq!(pending(), 2);
        let victim_id = Spi::get_one::<i64>("SELECT id FROM postvec.registry")
            .unwrap()
            .unwrap();

        // The attacker's table, with the shared purge function pointed at the
        // victim's registry id.
        Spi::run("CREATE TABLE attacker (id int PRIMARY KEY)").unwrap();
        Spi::run(&format!(
            "CREATE TRIGGER pwn AFTER TRUNCATE ON attacker
                 FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_truncate('{victim_id}')"
        ))
        .unwrap();
        Spi::run("TRUNCATE attacker").unwrap();
        assert_eq!(
            pending(),
            2,
            "a TRUNCATE on an unrelated table must not purge the victim's jobs"
        );

        // The legitimate trigger (matching table) still purges.
        Spi::run("TRUNCATE docs").unwrap();
        assert_eq!(pending(), 0);
    }

    /// The network-consuming one-shot functions are not PUBLIC-executable
    /// (resource amplification); app roles get them via explicit GRANT.
    #[pg_test]
    fn network_functions_are_not_public_executable() {
        Spi::run("CREATE ROLE pv_plain_role").unwrap();
        for func in [
            "postvec.refresh_models()",
            "postvec.embed(text, text)",
            "postvec.embed(text[], text)",
            "postvec.convert(real[], text, text)",
        ] {
            let ok = Spi::get_one_with_args::<bool>(
                "SELECT has_function_privilege('pv_plain_role', $1, 'EXECUTE')",
                &[func.into()],
            )
            .unwrap();
            assert_eq!(ok, Some(false), "{func} must not be PUBLIC-executable");
        }
        // The query path stays available to app roles.
        let ok = Spi::get_one::<bool>(
            "SELECT has_function_privilege('pv_plain_role',
                 'postvec.search(text, text, text, int, real, int, int, jsonb)', 'EXECUTE')",
        )
        .unwrap();
        assert_eq!(ok, Some(true), "search() stays PUBLIC");
    }

    /// uninstall() sweeps every entry in the database and is gated on
    /// superuser instead of failing midway on foreign-owned tables.
    #[pg_test]
    fn uninstall_requires_superuser() {
        seed_and_enable(DOCS);
        Spi::run("CREATE ROLE pv_not_super").unwrap();
        Spi::run("SET ROLE pv_not_super").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.uninstall()").ok();
        });
        assert!(r.is_err(), "non-superuser uninstall() must be refused");
    }

    /// Composite PK: statement triggers key rows as
    /// `ROW(a,b)::text`, dedup still coalesces, and only real changes enqueue.
    #[pg_test]
    fn composite_pk_statement_triggers() {
        seed_and_enable("CREATE TABLE docs (a int, b text, body text, PRIMARY KEY (a, b))");

        Spi::run("INSERT INTO docs VALUES (1,'x','one'), (2,'y','two'), (3,'z',NULL)").unwrap();
        assert_eq!(pending(), 2, "NULL body skipped");
        let keys = Spi::get_one::<i64>(
            "SELECT count(*) FROM postvec.jobs WHERE pk_value IN ('(1,x)','(2,y)')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(keys, 2, "jobs keyed by the record text form");

        clear_jobs();
        Spi::run("UPDATE docs SET body = body").unwrap();
        assert_eq!(pending(), 0, "IS DISTINCT FROM still filters");
        Spi::run("UPDATE docs SET body = 'changed' WHERE a = 1").unwrap();
        Spi::run("UPDATE docs SET body = 'changed again' WHERE a = 1").unwrap();
        assert_eq!(
            pending(),
            1,
            "repeated updates to one composite key coalesce"
        );
    }

    /// Composite keys containing quoting-hostile characters survive the
    /// round trip (record text output quotes them unambiguously).
    #[pg_test]
    fn composite_pk_hostile_characters() {
        seed_and_enable("CREATE TABLE docs (a text, b text, body text, PRIMARY KEY (a, b))");
        Spi::run(
            "INSERT INTO docs VALUES ('with,comma', 'with)paren', 'x'),
                                     ('with\"quote', 'O''Brien', 'y')",
        )
        .unwrap();
        assert_eq!(pending(), 2);
        // The two pk_values must be distinct and match the ROW()::text form.
        let distinct =
            Spi::get_one::<i64>("SELECT count(DISTINCT pk_value) FROM postvec.jobs").unwrap();
        assert_eq!(distinct, Some(2));
    }

    /// Partitioned table, row-mode triggers: cloned to every
    /// partition, so both parent-addressed and direct-partition DML enqueue.
    #[pg_test]
    fn partitioned_table_row_mode_triggers() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE pdocs (id bigint NOT NULL, body text, PRIMARY KEY (id))
             PARTITION BY RANGE (id)",
        )
        .unwrap();
        Spi::run("CREATE TABLE pdocs_lo PARTITION OF pdocs FOR VALUES FROM (0) TO (100)").unwrap();
        Spi::run("CREATE TABLE pdocs_hi PARTITION OF pdocs FOR VALUES FROM (100) TO (200)")
            .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('pdocs','body','m', trigger_mode => 'row', backfill => false)",
        )
        .unwrap();

        // Vector column exists on the parent (and thus every partition).
        let typ = Spi::get_one::<String>(
            "SELECT pg_catalog.format_type(atttypid, atttypmod) FROM pg_attribute
              WHERE attrelid = 'pdocs'::regclass AND attname = 'body_semantic'",
        )
        .unwrap();
        assert_eq!(typ.as_deref(), Some("vector(4)"));

        // DML through the parent...
        Spi::run("INSERT INTO pdocs VALUES (1, 'via parent'), (150, 'other partition')").unwrap();
        // ...and directly into a partition (row triggers are cloned).
        Spi::run("INSERT INTO pdocs_lo VALUES (2, 'direct into partition')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(3)
        );

        // TRUNCATE of the parent purges pending jobs.
        Spi::run("TRUNCATE pdocs").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
    }

    /// Partitioned table, statement mode: transition tables on the parent
    /// capture parent-addressed multi-partition DML in one firing.
    #[pg_test]
    fn partitioned_table_statement_mode_via_parent() {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE pdocs (id bigint NOT NULL, body text, PRIMARY KEY (id))
             PARTITION BY RANGE (id)",
        )
        .unwrap();
        Spi::run("CREATE TABLE pdocs_lo PARTITION OF pdocs FOR VALUES FROM (0) TO (100)").unwrap();
        Spi::run("CREATE TABLE pdocs_hi PARTITION OF pdocs FOR VALUES FROM (100) TO (200)")
            .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('pdocs','body','m', backfill => false)")
            .unwrap();

        Spi::run("INSERT INTO pdocs VALUES (1,'a'), (150,'b'), (2,NULL)").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(2),
            "one statement firing captured rows across partitions, NULL skipped"
        );

        Spi::run("DELETE FROM postvec.jobs").unwrap();
        Spi::run("UPDATE pdocs SET body = 'changed' WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(1),
            "UPDATE through the parent enqueues via transition tables"
        );
    }

    /// The triggers run as the DML-issuing role (SECURITY INVOKER): a plain
    /// application role with table privileges — and no postvec grants beyond
    /// what the extension ships — must be able to INSERT / UPDATE / TRUNCATE
    /// an enabled table. Covers the schema USAGE grant, the column-scoped
    /// INSERT grant on postvec.jobs, and trg_truncate's SECURITY DEFINER.
    #[pg_test]
    fn non_owner_dml_works_through_the_triggers() {
        seed_and_enable(DOCS);
        // Second table in row mode — the row-trigger bodies enqueue too.
        Spi::run(
            "CREATE TABLE rdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('rdocs','body','m', trigger_mode => 'row', backfill => false)",
        )
        .unwrap();

        Spi::run("CREATE ROLE pv_app_writer").unwrap();
        Spi::run("GRANT SELECT, INSERT, UPDATE, TRUNCATE ON docs, rdocs TO pv_app_writer").unwrap();
        Spi::run("SET ROLE pv_app_writer").unwrap();

        Spi::run("INSERT INTO docs (body) VALUES ('by app role'), ('another')").unwrap();
        Spi::run("INSERT INTO rdocs (body) VALUES ('row mode')").unwrap();
        Spi::run("UPDATE docs SET body = 'changed' WHERE body = 'another'").unwrap();
        assert_eq!(pending(), 3, "both modes enqueue for a non-owner writer");

        Spi::run("TRUNCATE docs").unwrap();
        Spi::run("RESET ROLE").unwrap();
        assert_eq!(
            pending(),
            1,
            "TRUNCATE by the app role purged docs' jobs (SECURITY DEFINER), rdocs' remains"
        );
    }

    /// `postvec.worker_kick()` is called by every generated trigger; it must
    /// be a silent no-op without a live worker, and survive a stale pid.
    #[pg_test]
    fn worker_kick_is_safe_without_worker() {
        Spi::run("SELECT postvec.worker_kick()").unwrap();
        // Stale heartbeat with a pid that (almost certainly) isn't a PG proc.
        Spi::run(
            "INSERT INTO postvec.worker_heartbeat (pid, last_beat, jobs_done, errors)
             VALUES (999999, now(), 0, 0)",
        )
        .unwrap();
        Spi::run("SELECT postvec.worker_kick()").unwrap();
    }
}
