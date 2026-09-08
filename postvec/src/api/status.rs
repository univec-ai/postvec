//! `postvec.status()`: one row per registry entry with queue depth, dead
//! count, oldest pending age, model staleness, last error and worker
//! liveness. `postvec.stats()`: the worker's lifetime counters plus queue
//! totals.

use pgrx::prelude::*;

#[pg_extern]
#[allow(clippy::type_complexity)]
fn status() -> TableIterator<
    'static,
    (
        name!(registry_id, i64),
        name!(relation, String),
        name!(source_column, String),
        name!(model, String),
        name!(dim, i32),
        name!(state, String),
        name!(distance, String),
        name!(backfill_mode, String),
        name!(pending_jobs, i64),
        name!(dead_jobs, i64),
        name!(oldest_pending_seconds, Option<f64>),
        name!(has_vector_index, bool),
        name!(model_last_seen, Option<String>),
        name!(last_error, Option<String>),
        name!(worker_pid, Option<i32>),
        name!(worker_last_beat, Option<String>),
        name!(index_mode, String),
        name!(index_error, Option<String>),
        name!(chunking, String),
        name!(chunk_size, Option<i32>),
        name!(chunk_overlap, Option<i32>),
        name!(destination, Option<String>),
        name!(destination_view, Option<String>),
        name!(pending_refresh_jobs, i64),
        name!(pending_embed_jobs, i64),
        name!(lexical_docs, i64),
        name!(lexical_stats_age_seconds, Option<f64>),
        name!(lexical_error, Option<String>),
        name!(space, Option<String>),
        name!(route, Option<String>),
        name!(route_execution, Option<String>),
    ),
> {
    // has_vector_index inlines registry::vector_index_probe_sql, the same
    // fragment vector_index_exists() and migration finalization use, so one
    // query serves all rows without a second implementation that can drift.
    // index_mode/index_error sit after the original columns so an `auto`
    // entry whose build failed is visible. The probe's relation is the
    // entry's vector target (the destination for a recursive entry); the
    // column stays `r.vector_column`, which is meaningful in both modes.
    let probe = crate::registry::vector_index_probe_sql(
        "to_regclass(quote_ident(COALESCE(r.destination_schema, r.table_schema)) || '.' || \
         quote_ident(COALESCE(r.destination_table, r.table_name)))",
        "r.vector_column",
    );
    let q = format!(
        "SELECT r.id,
                        r.table_schema || '.' || r.table_name AS relation,
                        r.source_column, r.model, r.dim, r.state, r.distance,
                        r.backfill_mode,
                        COALESCE(j.pending, 0)::bigint,
                        COALESCE(jd.dead, 0)::bigint,
                        EXTRACT(EPOCH FROM (now() - j.oldest))::float8,
                        COALESCE({probe}, false) AS has_vector_index,
                        mm.last_seen::text,
                        je.last_error,
                        hb.pid, hb.last_beat::text,
                        r.index_mode, r.index_error,
                        r.chunking, r.chunk_size, r.chunk_overlap,
                        CASE WHEN r.destination_table IS NOT NULL
                             THEN r.destination_schema || '.' || r.destination_table
                        END AS destination,
                        CASE WHEN r.destination_view IS NOT NULL
                             THEN r.destination_schema || '.' || r.destination_view
                        END AS destination_view,
                        COALESCE(j.pending_refresh, 0)::bigint,
                        COALESCE(j.pending_embed, 0)::bigint,
                        COALESCE(ls.n, 0)::bigint,
                        EXTRACT(EPOCH FROM (now() - ls.refreshed_at))::float8,
                        ls.error,
                        COALESCE(mm.space, r.space),
                        mm.route,
                        mm.route_execution
                   FROM postvec.registry r
                   LEFT JOIN postvec.lexical_stats ls ON ls.registry_id = r.id
                   LEFT JOIN (
                        SELECT registry_id, count(*) AS pending, min(created_at) AS oldest,
                               count(*) FILTER (WHERE op = 'refresh') AS pending_refresh,
                               count(*) FILTER (WHERE op = 'embed') AS pending_embed
                          FROM postvec.jobs GROUP BY registry_id
                   ) j ON j.registry_id = r.id
                   LEFT JOIN LATERAL (
                        -- Most recent error via a top-1 ordered fetch.
                        -- array_agg(... ORDER BY) would materialize and sort
                        -- every pending error string per group just to take
                        -- element [1], which spikes memory on a large backlog.
                        SELECT last_error FROM postvec.jobs
                         WHERE registry_id = r.id AND last_error IS NOT NULL
                         ORDER BY not_before DESC
                         LIMIT 1
                   ) je ON true
                   LEFT JOIN (
                        SELECT registry_id, count(*) AS dead
                          FROM postvec.jobs_dead GROUP BY registry_id
                   ) jd ON jd.registry_id = r.id
                   LEFT JOIN LATERAL (
                        -- The served embed route, else the converter that
                        -- targets the entry's space (an embed-bridge entry).
                        SELECT last_seen, space, route, route_execution FROM (
                            SELECT 0 AS tier, last_seen, space, route, execution AS route_execution
                              FROM postvec._route(r.model, r.space)
                            UNION ALL
                            SELECT 1, last_seen, target_model, name, 'bridge'
                              FROM postvec.models
                             WHERE model_type = 'convert' AND target_model IN (r.model, r.space)
                        ) x ORDER BY tier, route LIMIT 1
                   ) mm ON true
                   LEFT JOIN (SELECT pid, last_beat FROM postvec.worker_heartbeat LIMIT 1) hb ON true
                  ORDER BY r.id"
    );
    let rows = Spi::connect(|c| {
        let t = c
            .select(q.as_str(), None, &[])
            .expect("postvec: status query failed");
        t.into_iter()
            .map(|r| {
                (
                    r.get::<i64>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap().unwrap(),
                    r.get::<String>(3).unwrap().unwrap(),
                    r.get::<String>(4).unwrap().unwrap(),
                    r.get::<i32>(5).unwrap().unwrap(),
                    r.get::<String>(6).unwrap().unwrap(),
                    r.get::<String>(7).unwrap().unwrap(),
                    r.get::<String>(8).unwrap().unwrap(),
                    r.get::<i64>(9).unwrap().unwrap(),
                    r.get::<i64>(10).unwrap().unwrap(),
                    r.get::<f64>(11).unwrap(),
                    r.get::<bool>(12).unwrap().unwrap_or(false),
                    r.get::<String>(13).unwrap(),
                    r.get::<String>(14).unwrap(),
                    r.get::<i32>(15).unwrap(),
                    r.get::<String>(16).unwrap(),
                    r.get::<String>(17).unwrap().unwrap(),
                    r.get::<String>(18).unwrap(),
                    r.get::<String>(19).unwrap().unwrap(),
                    r.get::<i32>(20).unwrap(),
                    r.get::<i32>(21).unwrap(),
                    r.get::<String>(22).unwrap(),
                    r.get::<String>(23).unwrap(),
                    r.get::<i64>(24).unwrap().unwrap_or(0),
                    r.get::<i64>(25).unwrap().unwrap_or(0),
                    r.get::<i64>(26).unwrap().unwrap_or(0),
                    r.get::<f64>(27).unwrap(),
                    r.get::<String>(28).unwrap(),
                    r.get::<String>(29).unwrap(),
                    r.get::<String>(30).unwrap(),
                    r.get::<String>(31).unwrap(),
                )
            })
            .collect::<Vec<_>>()
    });
    TableIterator::new(rows)
}

/// Cluster-wide counters: the worker's lifetime tallies (from the heartbeat
/// row) plus current queue totals. One row; all zeros/NULLs when the worker
/// has not run yet. Prometheus scraping rides the deployment's existing
/// postgres_exporter — point a custom query at this function.
#[pg_extern]
#[allow(clippy::type_complexity)]
fn stats() -> TableIterator<
    'static,
    (
        name!(worker_pid, Option<i32>),
        name!(worker_started_at, Option<String>),
        name!(worker_last_beat, Option<String>),
        name!(jobs_embedded, i64),
        name!(jobs_nulled, i64),
        name!(jobs_retried, i64),
        name!(jobs_dead_lettered, i64),
        name!(migration_rows_converted, i64),
        name!(migration_rows_skipped, i64),
        name!(model_refreshes, i64),
        name!(worker_errors, i64),
        name!(worker_last_error, Option<String>),
        name!(queue_pending, i64),
        name!(queue_claimed, i64),
        name!(queue_dead, i64),
        name!(migrations_running, i64),
        name!(documents_chunked, i64),
        name!(chunks_created, i64),
    ),
> {
    let row = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT hb.pid, hb.started_at::text, hb.last_beat::text,
                        COALESCE(hb.jobs_embedded, 0), COALESCE(hb.jobs_nulled, 0),
                        COALESCE(hb.jobs_retried, 0), COALESCE(hb.jobs_dead, 0),
                        COALESCE(hb.rows_converted, 0), COALESCE(hb.rows_skipped, 0),
                        COALESCE(hb.model_refreshes, 0), COALESCE(hb.errors, 0),
                        hb.last_error,
                        (SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL),
                        (SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NOT NULL),
                        (SELECT count(*) FROM postvec.jobs_dead),
                        (SELECT count(*) FROM postvec.migrations WHERE state = 'running'),
                        COALESCE(hb.documents_chunked, 0), COALESCE(hb.chunks_created, 0)
                   FROM (SELECT 1) one
                   LEFT JOIN (SELECT * FROM postvec.worker_heartbeat LIMIT 1) hb ON true",
                Some(1),
                &[],
            )
            .expect("postvec: stats query failed");
        t.into_iter()
            .map(|r| {
                (
                    r.get::<i32>(1).unwrap(),
                    r.get::<String>(2).unwrap(),
                    r.get::<String>(3).unwrap(),
                    r.get::<i64>(4).unwrap().unwrap_or(0),
                    r.get::<i64>(5).unwrap().unwrap_or(0),
                    r.get::<i64>(6).unwrap().unwrap_or(0),
                    r.get::<i64>(7).unwrap().unwrap_or(0),
                    r.get::<i64>(8).unwrap().unwrap_or(0),
                    r.get::<i64>(9).unwrap().unwrap_or(0),
                    r.get::<i64>(10).unwrap().unwrap_or(0),
                    r.get::<i64>(11).unwrap().unwrap_or(0),
                    r.get::<String>(12).unwrap(),
                    r.get::<i64>(13).unwrap().unwrap_or(0),
                    r.get::<i64>(14).unwrap().unwrap_or(0),
                    r.get::<i64>(15).unwrap().unwrap_or(0),
                    r.get::<i64>(16).unwrap().unwrap_or(0),
                    r.get::<i64>(17).unwrap().unwrap_or(0),
                    r.get::<i64>(18).unwrap().unwrap_or(0),
                )
            })
            .collect::<Vec<_>>()
    });
    TableIterator::new(row)
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// Seed model 'm' (dim 4), create docs with `rows` bodies, and enable it.
    fn setup_docs(rows: i32) {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',4,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::run_with_args(
            "INSERT INTO docs (body) SELECT 'row ' || g FROM generate_series(1, $1) g",
            &[rows.into()],
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')").unwrap();
    }

    #[pg_test]
    fn status_counts_match() {
        setup_docs(3);

        // 3 backfill jobs enqueued for the one entry.
        let pending = Spi::get_one::<i64>("SELECT pending_jobs FROM postvec.status()").unwrap();
        assert_eq!(pending, Some(3));
        let dead = Spi::get_one::<i64>("SELECT dead_jobs FROM postvec.status()").unwrap();
        assert_eq!(dead, Some(0));
        let model = Spi::get_one::<String>("SELECT model FROM postvec.status()").unwrap();
        assert_eq!(model.as_deref(), Some("m"));
        let backfill =
            Spi::get_one::<String>("SELECT backfill_mode FROM postvec.status()").unwrap();
        assert_eq!(backfill.as_deref(), Some("queue"));
    }

    /// `space`/`route`/`route_execution` follow `postvec._route()`: the
    /// route in use, and the entry's remembered space once it is served by
    /// another route of that space.
    #[pg_test]
    fn status_reports_space_and_route() {
        setup_docs(0);
        let row = Spi::get_one::<String>(
            "SELECT space || '|' || route || '|' || route_execution FROM postvec.status()",
        )
        .unwrap();
        assert_eq!(row.as_deref(), Some("m|m|local"));
        Spi::run(
            "UPDATE postvec.models SET name = 'hosted-m', target_model = 'm',
                    raw = '{\"extra\":{\"provider\":\"openai\"}}' WHERE name = 'm'",
        )
        .unwrap();
        let row = Spi::get_one::<String>(
            "SELECT space || '|' || route || '|' || route_execution FROM postvec.status()",
        )
        .unwrap();
        assert_eq!(row.as_deref(), Some("m|hosted-m|provider openai"));
    }

    /// A convert-only model (embed-bridge routed entry) has no embed row in
    /// the cache — its staleness must be reported through the converter that
    /// targets it, not as a NULL `model_last_seen`.
    #[pg_test]
    fn bridged_entry_reports_model_last_seen() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('m', 'embed', NULL, 'm', 3, '{}'::jsonb),
                    ('conv-m-ext', 'convert', 'm', 'ext', 4, '{}'::jsonb),
                    ('embed-bridge', 'embed-bridge', NULL, NULL, NULL, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE bdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('bdocs','body','ext', backfill => false)")
            .unwrap();
        let seen = Spi::get_one::<String>("SELECT model_last_seen FROM postvec.status()").unwrap();
        assert!(
            seen.is_some(),
            "bridged entry model staleness comes from the converter row"
        );
    }

    /// An expression index on a *sibling* column whose name contains the
    /// vector column as a prefix (exactly what migrate()'s `<col>_new`
    /// creates) must not count as the entry's vector index.
    #[pg_test]
    fn sibling_column_expression_index_is_not_a_false_positive() {
        setup_docs(0);

        // A migration-style sibling column with its own expression index.
        Spi::run("ALTER TABLE docs ADD COLUMN body_semantic_new vector(4)").unwrap();
        Spi::run(
            "CREATE INDEX docs_new_halfvec ON docs
              USING hnsw ((body_semantic_new::halfvec(4)) halfvec_cosine_ops)",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT has_vector_index FROM postvec.status()").unwrap(),
            Some(false),
            "the sibling column's index must not match body_semantic"
        );

        // The entry's own expression index does count.
        Spi::run(
            "CREATE INDEX docs_own_halfvec ON docs
              USING hnsw ((body_semantic::halfvec(4)) halfvec_cosine_ops)",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT has_vector_index FROM postvec.status()").unwrap(),
            Some(true)
        );
    }

    /// A non-ANN index that merely references the vector column (a btree
    /// diagnostic expression index) must not count as a vector-search
    /// index. pgvector's ANN access methods (hnsw shown elsewhere, ivfflat
    /// here) do.
    #[pg_test]
    fn non_ann_dependent_index_is_not_a_vector_index() {
        setup_docs(0);

        Spi::run("CREATE INDEX docs_diag ON docs ((vector_dims(body_semantic)))").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT has_vector_index FROM postvec.status()").unwrap(),
            Some(false),
            "a btree expression index over the column is not an ANN index"
        );

        Spi::run(
            "CREATE INDEX docs_ivf ON docs
              USING ivfflat (body_semantic vector_cosine_ops) WITH (lists = 1)",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT has_vector_index FROM postvec.status()").unwrap(),
            Some(true),
            "ivfflat counts as an ANN index"
        );
    }

    #[pg_test]
    fn stats_reports_queue_and_worker_row() {
        // No worker in the test harness: worker columns are NULL/0, queue
        // counts still live.
        setup_docs(2);

        let pending = Spi::get_one::<i64>("SELECT queue_pending FROM postvec.stats()").unwrap();
        assert_eq!(pending, Some(2));
        let pid = Spi::get_one::<i32>("SELECT worker_pid FROM postvec.stats()").unwrap();
        assert_eq!(pid, None, "no worker heartbeat in the test cluster");

        // With a heartbeat row present, the counters surface.
        Spi::run(
            "INSERT INTO postvec.worker_heartbeat
                 (pid, last_beat, started_at, jobs_done, errors, jobs_embedded)
             VALUES (4242, now(), now(), 7, 1, 7)",
        )
        .unwrap();
        let embedded = Spi::get_one::<i64>("SELECT jobs_embedded FROM postvec.stats()").unwrap();
        assert_eq!(embedded, Some(7));
        let pid = Spi::get_one::<i32>("SELECT worker_pid FROM postvec.stats()").unwrap();
        assert_eq!(pid, Some(4242));
    }
}
