//! Every SQL statement the CLI issues.
//!
//! All constant. The single exception is `CREATE DATABASE`, which PostgreSQL
//! neither parameterizes nor allows in a transaction; it is rendered through
//! [`crate::validate::quote_identifier`] at the call site in [`super::local`].

/// Always-readable server facts.
pub const SERVER_FACTS: &str = "\
SELECT current_setting('server_version_num')::int AS version_num,
       current_setting('server_version')          AS version,
       current_setting('port')::int               AS port,
       pg_postmaster_start_time()::text           AS start_time,
       to_char(pg_postmaster_start_time() AT TIME ZONE 'UTC',
               'YYYY-MM-DD HH24:MI:SS.US')        AS start_exact";

/// Paths. Restricted to superusers and `pg_read_all_settings`, so this runs
/// separately and is allowed to fail.
pub const SERVER_PATHS: &str = "\
SELECT current_setting('data_directory') AS data_directory,
       current_setting('config_file')    AS config_file,
       current_setting('hba_file')       AS hba_file";

/// The cluster's system identifier: generated at `initdb` time and shared by
/// everything in that cluster's replication lineage.
///
/// Necessary but *not* sufficient to identify an instance: a physical standby
/// or a copied data directory carries its primary's identifier, so this alone
/// would happily match a primary against its own standby. It is paired with the
/// exact postmaster start time to identify which instance, not merely which
/// lineage. Restricted like the paths above, so it may fail.
pub const SYSTEM_IDENTIFIER: &str =
    "SELECT system_identifier::text AS system_identifier FROM pg_control_system()";

pub const SETTINGS: &str = "\
SELECT name, setting, context, source, sourcefile, sourceline, pending_restart
  FROM pg_settings
 WHERE name = ANY($1)
 ORDER BY name";

/// Both the named settings and every row the server could not parse: a broken
/// unrelated line still stops the cluster from starting.
pub const FILE_SETTINGS: &str = "\
SELECT name,
       setting,
       COALESCE(sourcefile, '') AS sourcefile,
       COALESCE(sourceline, 0)  AS sourceline,
       applied,
       error
  FROM pg_file_settings
 WHERE name = ANY($1) OR error IS NOT NULL
 ORDER BY sourcefile, sourceline";

pub const DATABASE_EXISTS: &str = "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)";

pub const AVAILABLE_EXTENSIONS: &str = "\
SELECT a.name, a.default_version, e.extversion AS installed_version
  FROM pg_available_extensions a
  LEFT JOIN pg_extension e ON e.extname = a.name
 WHERE a.name IN ('postvec', 'vector')
 ORDER BY a.name";

pub const CREATE_EXTENSION: &str = "CREATE EXTENSION IF NOT EXISTS postvec CASCADE";

pub const EXTENSION_PRESENT: &str = "SELECT extversion FROM pg_extension WHERE extname = 'postvec'";

/// `postvec.build_info()` only exists from the version that added the
/// diagnostics contract; probe before calling it.
pub const HAS_BUILD_INFO: &str =
    "SELECT to_regprocedure('postvec.build_info()') IS NOT NULL AS present";

// Two-argument uninstall (drop_columns, drop_destinations). An older
// one-argument extension correctly gets the "upgrade postvec" refusal.
pub const HAS_UNINSTALL: &str =
    "SELECT to_regprocedure('postvec.uninstall(boolean, boolean)') IS NOT NULL AS present";

pub const LIBRARY_VERSION: &str = "SELECT postvec.version() AS version";

pub const BUILD_INFO: &str = "SELECT postvec.build_info() AS info";

pub const UNINSTALL_ENTRIES: &str =
    "SELECT postvec.uninstall(drop_columns => $1, drop_destinations => $2) AS cleaned";

/// Every database. Templates can carry the extension (installing into
/// `template1` is a standard way to make new databases inherit it), and a
/// database can be set `datallowconn = false` after the fact — both are the
/// caller's problem to report, never to hide.
pub const LIST_DATABASES: &str = "\
SELECT datname, datallowconn AS allow_conn, datistemplate AS is_template
  FROM pg_database
 ORDER BY datname";

/// The chunk destinations the registry names, read through `to_jsonb` so an
/// older schema without the columns yields no rows instead of an error.
pub const REGISTRY_DESTINATIONS: &str = "\
SELECT (to_jsonb(r) ->> 'destination_schema') AS destination_schema,
       (to_jsonb(r) ->> 'destination_table')  AS destination_table
  FROM postvec.registry r
 WHERE to_jsonb(r) ->> 'destination_table' IS NOT NULL
 ORDER BY 1, 2";

pub const RELATION_EXISTS: &str =
    "SELECT to_regclass(quote_ident($1) || '.' || quote_ident($2)) IS NOT NULL AS present";

/// No CASCADE, ever: it would silently drop objects the operator did not ask
/// about.
pub const DROP_EXTENSION: &str = "DROP EXTENSION postvec";

pub const RELOAD_CONF: &str = "SELECT pg_reload_conf() AS reloaded";

pub const REFRESH_MODELS: &str = "SELECT postvec.refresh_models() AS models";
pub const START_WORKER: &str = "SELECT postvec.start_worker() AS started";

pub const HEARTBEAT: &str = "\
SELECT pid,
       EXTRACT(EPOCH FROM (now() - last_beat))::float8 AS age_s,
       started_at::text                                AS started_at,
       last_error,
       COALESCE(errors, 0)          AS errors,
       COALESCE(jobs_embedded, 0)   AS jobs_embedded,
       COALESCE(jobs_dead, 0)       AS jobs_dead,
       COALESCE(model_refreshes, 0) AS model_refreshes
  FROM postvec.worker_heartbeat
 LIMIT 1";

pub const HEARTBEAT_SAMPLE: &str = "\
SELECT pid, EXTRACT(EPOCH FROM (now() - last_beat))::float8 AS age_s
  FROM postvec.worker_heartbeat
 LIMIT 1";

/// Background workers appear in `pg_stat_activity`, so a recorded pid can be
/// confirmed against the cluster rather than merely against `/proc`.
pub const PID_IS_LIVE: &str = "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1)";

pub const QUEUE_TOTALS: &str = "\
SELECT COALESCE(count(*) FILTER (WHERE claimed_at IS NULL), 0)::bigint     AS pending,
       COALESCE(count(*) FILTER (WHERE claimed_at IS NOT NULL), 0)::bigint AS claimed,
       EXTRACT(EPOCH FROM (now() - min(created_at) FILTER (WHERE claimed_at IS NULL)))::float8
           AS oldest_pending_s
  FROM postvec.jobs";

pub const DEAD_LETTERS: &str = "\
SELECT count(*)::bigint AS dead,
       COALESCE(
           (array_agg(DISTINCT left(last_error, 160))
                FILTER (WHERE last_error IS NOT NULL))[1:3],
           '{}'::text[]
       ) AS reasons
  FROM postvec.jobs_dead";

/// Registry entries joined to `postvec.status()` for queue/index state, plus
/// the catalog checks `status()` does not make: does the table still exist, do
/// both columns still exist, are the generated triggers still attached.
pub const REGISTRY_ENTRIES: &str = "\
WITH reg AS (
    SELECT r.*,
           to_regclass(quote_ident(r.table_schema) || '.' || quote_ident(r.table_name)) AS rel,
           -- The vector target: the managed chunk destination for a
           -- recursive entry, the source table otherwise. Read through
           -- to_jsonb so an older extension schema (no destination columns)
           -- degrades to the source table instead of a query error.
           to_regclass(quote_ident(COALESCE(to_jsonb(r) ->> 'destination_schema',
                                            r.table_schema)) || '.' ||
                       quote_ident(COALESCE(to_jsonb(r) ->> 'destination_table',
                                            r.table_name))) AS vec_rel
      FROM postvec.registry r
)
SELECT reg.id,
       reg.table_schema || '.' || reg.table_name AS relation,
       reg.source_column,
       reg.vector_column,
       reg.model,
       reg.dim,
       reg.state,
       COALESCE(s.pending_jobs, 0)      AS pending_jobs,
       COALESCE(s.dead_jobs, 0)         AS dead_jobs,
       COALESCE(s.has_vector_index, false) AS has_vector_index,
       -- Index-mode columns, read through to_jsonb so an older extension
       -- schema (which lacks them) yields the defaults instead of a query error.
       COALESCE(to_jsonb(reg) ->> 'index_mode', 'manual') AS index_mode,
       to_jsonb(reg) ->> 'index_error'                    AS index_error,
       -- Readiness as search() defines it: one valid/ready/live ANN index on
       -- the vector column (direct key or expression dependency) whose
       -- opclass matches the entry's distance and dimension. Ownership-
       -- neutral: a user-built index counts.
       EXISTS (
           SELECT 1
             FROM pg_index i
             JOIN pg_class ic ON ic.oid = i.indexrelid
             JOIN pg_am am ON am.oid = ic.relam
             JOIN pg_opclass oc ON oc.oid = ANY(i.indclass)
            WHERE i.indrelid = reg.vec_rel
              AND am.amname IN ('hnsw', 'ivfflat')
              AND i.indisvalid AND i.indisready AND i.indislive
              AND oc.opcname = CASE
                    WHEN reg.dim > 2000 THEN CASE reg.distance
                        WHEN 'l2' THEN 'halfvec_l2_ops'
                        WHEN 'ip' THEN 'halfvec_ip_ops'
                        ELSE 'halfvec_cosine_ops' END
                    ELSE CASE reg.distance
                        WHEN 'l2' THEN 'vector_l2_ops'
                        WHEN 'ip' THEN 'vector_ip_ops'
                        ELSE 'vector_cosine_ops' END
                  END
              AND (
                  EXISTS (SELECT 1 FROM pg_attribute a
                           WHERE a.attrelid = i.indrelid
                             AND a.attnum = ANY(i.indkey)
                             AND a.attname = reg.vector_column)
                  OR EXISTS (SELECT 1 FROM pg_depend d
                              JOIN pg_attribute a
                                ON a.attrelid = i.indrelid
                               AND a.attnum = d.refobjsubid
                             WHERE d.classid = 'pg_class'::regclass
                               AND d.objid = i.indexrelid
                               AND d.refclassid = 'pg_class'::regclass
                               AND d.refobjid = i.indrelid
                               AND a.attname = reg.vector_column)
              )
       ) AS has_expected_opclass_index,
       s.last_error,
       (reg.rel IS NOT NULL)            AS relation_exists,
       EXISTS (SELECT 1 FROM pg_attribute a
                WHERE a.attrelid = reg.rel AND a.attname = reg.source_column
                  AND a.attnum > 0 AND NOT a.attisdropped) AS source_column_exists,
       EXISTS (SELECT 1 FROM pg_attribute a
                WHERE a.attrelid = reg.vec_rel AND a.attname = reg.vector_column
                  AND a.attnum > 0 AND NOT a.attisdropped) AS vector_column_exists,
       (SELECT count(*)::bigint FROM pg_trigger t
         WHERE t.tgrelid = reg.rel AND NOT t.tgisinternal
           AND t.tgname LIKE 'postvec\\_%')                 AS trigger_count,
       -- The managed chunk destination of a recursive entry (NULL for a
       -- column-mode entry, and on an older schema without the columns).
       CASE WHEN to_jsonb(reg) ->> 'destination_table' IS NOT NULL
            THEN (to_jsonb(reg) ->> 'destination_schema') || '.' ||
                 (to_jsonb(reg) ->> 'destination_table')
       END                                                  AS destination
  FROM reg
  LEFT JOIN postvec.status() s ON s.registry_id = reg.id
 ORDER BY reg.id";

pub const MODEL_CACHE: &str = "\
SELECT count(*)::bigint AS count,
       EXTRACT(EPOCH FROM (now() - GREATEST(
           max(last_seen),
           (SELECT max(models_refreshed_at) FROM postvec.worker_heartbeat)
       )))::float8 AS newest_last_seen_s,
       COALESCE(array_agg(name ORDER BY name), '{}'::text[]) AS names
  FROM postvec.models";

pub const OPEN_MIGRATIONS: &str = "\
SELECT id,
       registry_id,
       state,
       rows_done,
       rows_total,
       error,
       EXTRACT(EPOCH FROM (now() - started_at))::float8 AS age_s,
       retry_failures
  FROM postvec.migrations
 WHERE state NOT IN ('done', 'aborted')
 ORDER BY id";

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole security posture of this module is "constant SQL only". A
    /// statement that grew a format placeholder is a review failure worth
    /// catching mechanically.
    #[test]
    fn statements_contain_no_format_placeholders() {
        for (name, sql) in [
            ("SERVER_FACTS", SERVER_FACTS),
            ("SERVER_PATHS", SERVER_PATHS),
            ("SYSTEM_IDENTIFIER", SYSTEM_IDENTIFIER),
            ("SETTINGS", SETTINGS),
            ("FILE_SETTINGS", FILE_SETTINGS),
            ("DATABASE_EXISTS", DATABASE_EXISTS),
            ("AVAILABLE_EXTENSIONS", AVAILABLE_EXTENSIONS),
            ("CREATE_EXTENSION", CREATE_EXTENSION),
            ("EXTENSION_PRESENT", EXTENSION_PRESENT),
            ("HAS_BUILD_INFO", HAS_BUILD_INFO),
            ("HAS_UNINSTALL", HAS_UNINSTALL),
            ("LIBRARY_VERSION", LIBRARY_VERSION),
            ("BUILD_INFO", BUILD_INFO),
            ("UNINSTALL_ENTRIES", UNINSTALL_ENTRIES),
            ("LIST_DATABASES", LIST_DATABASES),
            ("REGISTRY_DESTINATIONS", REGISTRY_DESTINATIONS),
            ("RELATION_EXISTS", RELATION_EXISTS),
            ("DROP_EXTENSION", DROP_EXTENSION),
            ("RELOAD_CONF", RELOAD_CONF),
            ("REFRESH_MODELS", REFRESH_MODELS),
            ("START_WORKER", START_WORKER),
            ("HEARTBEAT", HEARTBEAT),
            ("HEARTBEAT_SAMPLE", HEARTBEAT_SAMPLE),
            ("PID_IS_LIVE", PID_IS_LIVE),
            ("QUEUE_TOTALS", QUEUE_TOTALS),
            ("DEAD_LETTERS", DEAD_LETTERS),
            ("REGISTRY_ENTRIES", REGISTRY_ENTRIES),
            ("MODEL_CACHE", MODEL_CACHE),
            ("OPEN_MIGRATIONS", OPEN_MIGRATIONS),
        ] {
            assert!(
                !has_brace_outside_string_literal(sql),
                "{name} has a brace outside a quoted literal and may be a format string"
            );
        }
    }

    /// SQL array literals legitimately contain braces (`'{}'::text[]`), so the
    /// scan only flags braces in the statement text itself.
    fn has_brace_outside_string_literal(sql: &str) -> bool {
        let mut in_string = false;
        for ch in sql.chars() {
            match ch {
                '\'' => in_string = !in_string,
                '{' | '}' if !in_string => return true,
                _ => {}
            }
        }
        false
    }

    #[test]
    fn brace_scan_distinguishes_literals_from_placeholders() {
        assert!(!has_brace_outside_string_literal("SELECT '{}'::text[]"));
        assert!(has_brace_outside_string_literal("SELECT {name}"));
    }

    #[test]
    fn destructive_statements_never_cascade() {
        assert!(!DROP_EXTENSION.to_uppercase().contains("CASCADE"));
    }

    #[test]
    fn trigger_name_pattern_escapes_the_underscore_wildcard() {
        // `postvec_%` unescaped would also match `postvecX...`; the registry
        // check must not count a foreign trigger as postvec's.
        assert!(REGISTRY_ENTRIES.contains("postvec\\_%"));
    }
}
