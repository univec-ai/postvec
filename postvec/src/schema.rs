//! Extension SQL shipped with CREATE EXTENSION.
//!
//! The model cache (`postvec.models`, `postvec.worker_heartbeat`) and the
//! control-plane tables (`registry`, `jobs`, `jobs_dead`, `migrations`) plus
//! the shared TRUNCATE trigger. The four user-data tables are registered
//! with `pg_catalog.pg_extension_config_dump(...)` so `pg_dump` carries
//! their rows. `postvec.models` is a refreshable cache and is not dumped.

use pgrx::{extension_sql, extension_sql_file};

extension_sql_file!("../sql/managed/models.sql", name = "postvec_models_table");

extension_sql_file!(
    "../sql/managed/control.sql",
    name = "postvec_control_tables",
    requires = ["postvec_models_table"]
);

extension_sql_file!(
    "../sql/managed/triggers.sql",
    name = "postvec_chunk_triggers",
    requires = ["postvec_control_tables"]
);

extension_sql_file!(
    "../sql/managed/lexical.sql",
    name = "postvec_lexical",
    requires = ["postvec_control_tables"]
);

extension_sql!(
    r#"SELECT pg_catalog.pg_extension_config_dump('postvec.registry', '');
SELECT pg_catalog.pg_extension_config_dump('postvec.jobs', '');
SELECT pg_catalog.pg_extension_config_dump('postvec.jobs_dead', '');
SELECT pg_catalog.pg_extension_config_dump('postvec.migrations', '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.registry', 'id')::regclass, '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.jobs', 'id')::regclass, '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.jobs_dead', 'dead_id')::regclass, '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.migrations', 'id')::regclass, '');
"#,
    name = "postvec_config_dump",
    requires = ["postvec_control_tables", "postvec_lexical"]
);

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn control_tables_exist() {
        for t in [
            "postvec.registry",
            "postvec.jobs",
            "postvec.jobs_dead",
            "postvec.migrations",
            "postvec.lexical_stats",
            "postvec.lexical_df",
            "postvec.models",
            "postvec.worker_heartbeat",
        ] {
            let oid =
                Spi::get_one_with_args::<pg_sys::Oid>("SELECT to_regclass($1)::oid", &[t.into()])
                    .unwrap();
            assert!(
                oid.is_some() && oid.unwrap() != pg_sys::Oid::INVALID,
                "{t} missing"
            );
        }
    }

    #[pg_test]
    fn config_dump_includes_control_tables_and_sequences() {
        let count = Spi::get_one::<i64>(
            "SELECT count(*)
               FROM pg_extension e
               CROSS JOIN LATERAL unnest(e.extconfig) cfg(oid)
               JOIN pg_class c ON c.oid = cfg.oid
              WHERE e.extname = 'postvec'
                AND c.relname IN (
                    'registry', 'jobs', 'jobs_dead', 'migrations',
                    'registry_id_seq', 'jobs_id_seq',
                    'jobs_dead_dead_id_seq', 'migrations_id_seq'
                )",
        )
        .unwrap();
        assert_eq!(count, Some(8));
    }

    /// Exact queue/dead index inventory. Claim, probe and invalidation
    /// queries are written against these shapes; a dropped or renamed one
    /// degrades them to sequential scans.
    #[pg_test]
    fn queue_and_dead_indexes_have_the_specified_shapes() {
        for (index, must_contain) in [
            (
                "jobs_pending_dedup",
                vec![
                    "UNIQUE",
                    "registry_id, op, pk_value, chunk_id",
                    "NULLS NOT DISTINCT",
                    "claimed_at IS NULL",
                ],
            ),
            (
                "jobs_embed_claim_order",
                vec!["not_before, id", "claimed_at IS NULL", "op = 'embed'"],
            ),
            (
                "jobs_refresh_claim_order",
                vec!["not_before, id", "claimed_at IS NULL", "op = 'refresh'"],
            ),
            ("jobs_reclaim", vec!["claimed_at IS NOT NULL"]),
            ("jobs_registry_pk", vec!["registry_id, pk_value"]),
            (
                "jobs_live_embed_registry",
                vec!["(registry_id)", "op = 'embed'"],
            ),
            ("jobs_dead_registry_pk", vec!["registry_id, pk_value"]),
        ] {
            let def = Spi::get_one_with_args::<String>(
                "SELECT pg_get_indexdef(to_regclass('postvec.' || $1))",
                &[index.into()],
            )
            .unwrap()
            .unwrap_or_default();
            for frag in must_contain {
                assert!(def.contains(frag), "{index} must contain {frag:?}: {def}");
            }
        }
        // No superseded index should survive.
        for gone in ["jobs_claim_order", "jobs_registry"] {
            assert_eq!(
                Spi::get_one_with_args::<bool>(
                    "SELECT to_regclass('postvec.' || $1) IS NULL",
                    &[gone.into()],
                )
                .unwrap(),
                Some(true),
                "superseded index {gone} must be gone"
            );
        }
    }

    /// The partial unique index collapses repeated pending jobs for one row
    /// into a single entry, while allowing a fresh job once the prior one is
    /// claimed (claimed_at IS NOT NULL leaves the predicate).
    #[pg_test]
    fn dedup_index_coalesces_pending() {
        Spi::run(
            "INSERT INTO postvec.registry
               (table_schema, table_name, source_column, vector_column,
                pk_columns, pk_types, model, dim)
             VALUES ('public','t','body','body_semantic',
                     ARRAY['id'], ARRAY['bigint'], 'm', 4)",
        )
        .unwrap();
        let rid = Spi::get_one::<i64>("SELECT id FROM postvec.registry LIMIT 1")
            .unwrap()
            .unwrap();

        let ins = format!(
            "INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ({rid}, '7')
             ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING"
        );
        Spi::run(&ins).unwrap();
        Spi::run(&ins).unwrap();
        Spi::run(&ins).unwrap();
        let pending =
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL")
                .unwrap();
        assert_eq!(
            pending,
            Some(1),
            "three pending inserts must coalesce to one"
        );

        // Claim it; a new pending job for the same row is now allowed.
        Spi::run("UPDATE postvec.jobs SET claimed_at = now()").unwrap();
        Spi::run(&ins).unwrap();
        let total = Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap();
        assert_eq!(
            total,
            Some(2),
            "a fresh job is allowed once the prior is claimed"
        );
    }
}
