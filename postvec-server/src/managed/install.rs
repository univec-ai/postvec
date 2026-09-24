// SPDX-License-Identifier: BUSL-1.1

use super::{Command, ConnectionArgs};
use anyhow::{bail, Context, Result};
use postvec_core::registry::quote_ident;
use sqlx::{postgres::PgConnectOptions, Connection, Executor, PgConnection, Row};
use std::{io::Read, str::FromStr, time::Duration};

/// `UPGRADES[i]` takes an installed schema from version `i + 1` to `i + 2`,
/// inside the install transaction. A change to a table or index in
/// `models.sql`/`control.sql` appends one, and so does a function whose
/// arguments or result change: the function files are re-applied with
/// `CREATE OR REPLACE` on every install, which keeps dependent views and
/// grants, so only a step may drop one. A step must tolerate its own starting
/// state (`IF EXISTS`) and never builds an index: it renames the old one aside
/// and `INDEXES` builds the replacement.
const UPGRADES: &[&str] = &[
    "ALTER TABLE postvec.registry ADD COLUMN IF NOT EXISTS space text;
    ALTER INDEX IF EXISTS postvec.jobs_embed_claim_order RENAME TO jobs_embed_claim_order_v1;
    DROP FUNCTION IF EXISTS postvec.search_with_vector(text,text,real[],text,integer,real,integer,integer,jsonb),
        postvec.status(), postvec._refresh_lexical_stats(bigint);",
    // search() and embed() take the proxy's vector; the key-format pin and the
    // rekeying of queued state are the extension's 0.3.0 upgrade, verbatim.
    concat!(
        "DROP FUNCTION IF EXISTS postvec.search(text,text,text,integer,real,integer,integer,jsonb),
            postvec.embed(text,text);",
        include_str!("../../../postvec/sql/postvec--0.2.0--0.3.0.sql")
    ),
];
/// Run after the install commits, one statement at a time, on every install:
/// `CONCURRENTLY`, so writers enqueueing jobs never wait on a build. Each is a
/// no-op once applied.
const INDEXES: &[&str] = &[
    "CREATE INDEX CONCURRENTLY IF NOT EXISTS jobs_embed_claim_order ON postvec.jobs (registry_id, not_before, id) WHERE claimed_at IS NULL AND op = 'embed'",
    "DROP INDEX CONCURRENTLY IF EXISTS postvec.jobs_embed_claim_order_v1",
];
pub(super) const VERSION: i32 = UPGRADES.len() as i32 + 1;
const MARKER: &str = "postvec managed schema";
const MODELS: &str = include_str!("../../../postvec/sql/managed/models.sql");
const ROUTES: &str = include_str!("../../../postvec/sql/managed/routes.sql");
const CONTROL: &str = include_str!("../../../postvec/sql/managed/control.sql");
const TRIGGERS: &str = include_str!("../../../postvec/sql/managed/triggers.sql");
const LEXICAL: &str = include_str!("../../../postvec/sql/managed/lexical.sql");
const FUNCTIONS: &str = include_str!("../../../postvec/sql/managed/functions.sql");

/// Advice for source tables, in schemas this role can use, whose owner it is
/// not a member of: per owner, the grant a superuser can run, or the
/// hand-over any managed platform allows (PostgreSQL 16+ lets only a
/// superuser grant membership in another role's owner). The hand-over's
/// GRANT needs ADMIN OPTION on this role, which its creator holds.
pub(super) const GRANTS: &str = "WITH t AS (
    SELECT r.rolname::text AS o, n.nspname::text AS nsp, c.relname::text AS rel,
           has_schema_privilege(n.oid, 'CREATE') AS can_create
      FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_catalog.pg_roles r ON r.oid = c.relowner
     WHERE c.relkind IN ('r','p') AND n.nspname NOT IN ('postvec','information_schema')
       AND n.nspname NOT LIKE 'pg_%' AND NOT pg_has_role(current_user, c.relowner, 'USAGE')
       AND has_schema_privilege(n.oid, 'USAGE')
       AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_class'::regclass
                        AND d.objid = c.oid AND d.deptype = 'e')
), admins AS (
    SELECT coalesce(string_agg(m.member::regrole::text, ' or ' ORDER BY m.member::regrole::text), 'a superuser') AS a
      FROM pg_catalog.pg_auth_members m
     WHERE m.roleid = (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = current_user) AND m.admin_option
)
SELECT coalesce(string_agg(block, E'\\n' ORDER BY o), '') FROM (
    SELECT o, format(E'-- as a superuser:\\nGRANT %1$I TO %2$I;\\n-- or hand the tables over, as %3$s:\\nGRANT %2$I TO %1$I;\\n%4$s-- then as %1$I:\\n%5$s',
        o, current_user, (SELECT a FROM admins),
        coalesce(E'-- as the schema owner:\\n' || string_agg(DISTINCT format(E'GRANT CREATE ON SCHEMA %I TO %I;\\n', nsp, current_user), '')
                 FILTER (WHERE NOT can_create), ''),
        string_agg(format('ALTER TABLE %I.%I OWNER TO %I;', nsp, rel, current_user), E'\\n' ORDER BY nsp, rel)) AS block
      FROM t GROUP BY o) blocks";

/// Endpoints known to pool per transaction, where the worker's session
/// advisory lock and LISTEN cannot work.
pub(super) fn transaction_pooler(options: &PgConnectOptions) -> bool {
    let host = options.get_host();
    ((host.ends_with(".supabase.com") || host.ends_with(".supabase.co"))
        && options.get_port() == 6543)
        || (host.ends_with(".neon.tech") && host.contains("-pooler."))
}
pub(super) const POOLER: &str = "this endpoint pools per transaction; the worker needs a direct or session-mode endpoint (Supabase: port 5432, Neon: the host without -pooler)";

/// A session advisory lock taken on this connection is visible from its next
/// statement, unless a transaction pooler moved the connection to another backend.
pub(super) async fn ensure_held(connection: &mut PgConnection, key: i32) -> Result<()> {
    let held: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_catalog.pg_locks WHERE locktype='advisory' AND pid=pg_backend_pid() AND classid=1886615158 AND objid=$1 AND objsubid=2 AND granted)")
        .bind(key)
        .fetch_one(connection)
        .await?;
    anyhow::ensure!(held, "{POOLER}");
    Ok(())
}

pub(super) fn dsn_has_password(dsn: &str) -> bool {
    dsn.to_ascii_lowercase().contains("password=")
        || reqwest::Url::parse(dsn)
            .ok()
            .is_some_and(|u| u.password().is_some())
}

pub(super) fn options(args: &ConnectionArgs) -> Result<PgConnectOptions> {
    let mut options = PgConnectOptions::from_str(&args.dsn)
        .map_err(|_| anyhow::anyhow!("invalid PostgreSQL DSN"))?;
    if options.get_application_name().is_none() {
        options = options.application_name("postvec-server");
    }
    if let Some(path) = &args.password_file {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .context("cannot open password file")?;
        let meta = file.metadata()?;
        if !meta.is_file()
            || meta.uid() != unsafe { libc::getuid() }
            || meta.mode() & 0o077 != 0
            || meta.len() > 65536
        {
            bail!(
                "password file must be a regular file owned by the current user, not readable by group or others (mode 0600), at most 65536 bytes"
            );
        }
        let mut password = String::new();
        (&mut file).take(65537).read_to_string(&mut password)?;
        if password.len() > 65536 {
            bail!("password file exceeds 65536 bytes");
        }
        let password = password.trim_end_matches(['\r', '\n']);
        if password.is_empty() || password.contains(['\r', '\n', '\0']) {
            bail!("password file must contain one nonempty password");
        }
        options = options.password(password);
    }
    Ok(options)
}

/// Settings every managed database session shares. Keepalives let the
/// database end a vanished client's session, and with it the leader lock, in
/// about a minute instead of the OS default hours.
/// The key format (`KEY_SETTINGS`) is the one the triggers queue keys in.
pub(super) async fn tune(connection: &mut PgConnection) -> Result<(), sqlx::Error> {
    let keys: String = postvec_core::registry::KEY_SETTINGS
        .iter()
        .map(|(name, value)| format!(", set_config('{name}', '{value}', false)"))
        .collect();
    connection
        .execute(
            format!("SELECT set_config('tcp_keepalives_idle', '30', false), set_config('tcp_keepalives_interval', '10', false),
                set_config('tcp_keepalives_count', '3', false), set_config('client_connection_check_interval', '10s', false){keys}")
            .as_str(),
        )
        .await?;
    Ok(())
}

pub(super) async fn connect(args: &ConnectionArgs) -> Result<PgConnection> {
    let options = options(args)?;
    let mut connection = tokio::time::timeout(
        Duration::from_secs(args.timeout.min(30).into()),
        PgConnection::connect_with(&options),
    )
    .await
    .context("database connection timed out")?
    .context("database connection failed")?;
    tune(&mut connection).await?;
    sqlx::query(
        "SELECT set_config('statement_timeout', $1, false), set_config('lock_timeout', $1, false)",
    )
    .bind(format!("{}s", args.timeout))
    .execute(&mut connection)
    .await?;
    Ok(connection)
}

/// Serializes `install` and `uninstall` on one database for the whole run,
/// post-commit index builds included. Polled between statements, never waited
/// on inside one: a backend blocked in `pg_advisory_lock` holds a snapshot, and
/// the other run's `CREATE INDEX CONCURRENTLY` waits for every older snapshot,
/// which deadlocks. Released when the connection closes.
async fn lock_installs(connection: &mut PgConnection, timeout: u32) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout.into());
    let mut noted = false;
    while !sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock(1886615158, 4)")
        .fetch_one(&mut *connection)
        .await?
    {
        if tokio::time::Instant::now() >= deadline {
            bail!("another managed install or uninstall is still running on this database; rerun when it finishes");
        }
        if !noted {
            eprintln!("Waiting for another managed install or uninstall on this database.");
            noted = true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    ensure_held(connection, 4).await
}

/// The installed managed schema version, `None` when there is no schema.
pub(super) async fn check(connection: &mut PgConnection) -> Result<Option<i32>> {
    let extension: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT FROM pg_catalog.pg_extension WHERE extname = 'postvec')",
    )
    .fetch_one(&mut *connection)
    .await?;
    if extension {
        bail!(
            "postvec extension is installed; managed commands cannot modify an extension database"
        );
    }
    let schema = sqlx::query(
        "SELECT obj_description(oid, 'pg_namespace') AS marker,
        pg_has_role(current_user, nspowner, 'USAGE') AS owned
        FROM pg_catalog.pg_namespace WHERE nspname = 'postvec'",
    )
    .fetch_optional(&mut *connection)
    .await?;
    let Some(schema) = schema else {
        return Ok(None);
    };
    if schema.get::<Option<String>, _>("marker").as_deref() != Some(MARKER)
        || !schema.get::<bool, _>("owned")
    {
        bail!("postvec schema is not a managed installation owned by this role");
    }
    let version: (i32, String) =
        sqlx::query_as("SELECT version, mode FROM postvec.schema_version WHERE id")
            .fetch_one(connection)
            .await
            .context("invalid managed schema version table")?;
    if version.1 != "managed" || !(1..=VERSION).contains(&version.0) {
        bail!(
            "unsupported managed schema version {}; this server supports 1 to {VERSION}; no changes made",
            version.0
        );
    }
    Ok(Some(version.0))
}

pub async fn run(command: Command) -> Result<()> {
    let args = match &command {
        Command::Install(a) | Command::Status(a) | Command::Uninstall(a) => a,
    };
    if dsn_has_password(&args.dsn) {
        eprintln!(
            "Database password in DSN; prefer --password-file to keep it out of process arguments."
        );
    }
    if transaction_pooler(&options(args)?) {
        bail!("{POOLER}");
    }
    let mut connection = connect(args).await?;
    let timeout = args.timeout;
    let result = execute(&mut connection, command, timeout).await;
    // Behind a transaction pooler the install lock would outlive this client
    // on a pooled backend; a direct session drops it on disconnect anyway.
    let _ = connection.execute("SELECT pg_advisory_unlock_all()").await;
    result
}

async fn execute(connection: &mut PgConnection, command: Command, timeout: u32) -> Result<()> {
    if !matches!(command, Command::Status(_)) {
        lock_installs(connection, timeout).await?;
    }
    let mut tx = connection.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(1886615158, 1)")
        .execute(&mut *tx)
        .await?;
    let installed = check(&mut tx).await?;
    match command {
        Command::Install(_) => {
            let vector: Option<(String, String, bool, bool, String)> = sqlx::query_as(
                "SELECT n.nspname::text, e.extversion::text, string_to_array(e.extversion, '.')::int[] >= '{0,8}',
                        has_schema_privilege(n.oid, 'USAGE'), current_user::text
                 FROM pg_catalog.pg_extension e JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'vector'",
            ).fetch_optional(&mut *tx).await?;
            let schema = match vector {
                None => bail!(
                    "pgvector is required; have the database administrator run CREATE EXTENSION vector first"
                ),
                Some((_, version, false, ..)) => {
                    bail!("pgvector {version} is installed; postvec needs pgvector 0.8 or newer")
                }
                Some((schema, .., false, role)) => bail!(
                    "pgvector is in schema {schema}, which this role cannot use; have the database administrator run GRANT USAGE ON SCHEMA {} TO {};",
                    quote_ident(&schema),
                    quote_ident(&role)
                ),
                Some((schema, ..)) => quote_ident(&schema),
            };
            // Loads pgvector into this backend; the SET hnsw.* clauses below are
            // otherwise unknown parameters a non-superuser cannot define.
            tx.execute(format!("SELECT {schema}.vector_dims('[0]'::{schema}.vector)").as_str())
                .await?;
            let platform = super::platform::detect(&mut tx).await?;
            match installed {
                None => {
                    tx.execute(
                        "CREATE SCHEMA postvec; REVOKE CREATE ON SCHEMA postvec FROM PUBLIC;
                        COMMENT ON SCHEMA postvec IS 'postvec managed schema'",
                    )
                    .await?;
                    for sql in [MODELS, CONTROL] {
                        tx.execute(sql).await?;
                    }
                }
                Some(version) => {
                    for sql in &UPGRADES[version as usize - 1..] {
                        tx.execute(*sql).await?;
                    }
                }
            }
            for sql in [ROUTES, TRIGGERS, LEXICAL, FUNCTIONS] {
                tx.execute(sql).await?;
            }
            sqlx::query("UPDATE postvec.schema_version SET version = $1, mode = 'managed'")
                .bind(VERSION)
                .execute(&mut *tx)
                .await?;
            tx.execute(include_str!("../../../postvec/sql/managed/lifecycle.sql"))
                .await?;
            sqlx::query(
                "INSERT INTO postvec.settings (key, value) VALUES ('platform', to_jsonb($1::text))
                ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind(&platform)
            .execute(&mut *tx)
            .await?;
            let grants: String = sqlx::query_scalar(GRANTS).fetch_one(&mut *tx).await?;
            tx.commit().await?;
            // An interrupted CONCURRENTLY build leaves an invalid index that
            // IF NOT EXISTS would skip; drop it so this run builds it again.
            // `lock_installs` still holds, so no other run can sweep this build.
            connection
                .execute("SET statement_timeout = 0; SET lock_timeout = 0")
                .await?;
            let invalid: Vec<String> = sqlx::query_scalar(
                "SELECT format('%I.%I', n.nspname, c.relname) FROM pg_catalog.pg_index i
                 JOIN pg_catalog.pg_class c ON c.oid = i.indexrelid
                 JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = 'postvec' AND NOT i.indisvalid",
            )
            .fetch_all(&mut *connection)
            .await?;
            for index in invalid {
                connection
                    .execute(format!("DROP INDEX CONCURRENTLY {index}").as_str())
                    .await?;
            }
            for sql in INDEXES {
                connection.execute(*sql).await?;
            }
            println!(
                "Managed schema v{VERSION} ready ({platform}). Configure managed[] or serve --sync to start the worker."
            );
            if grants.is_empty() {
                println!(
                    "The worker role already owns, or inherits ownership of, the existing source tables."
                );
            } else {
                println!("Tables owned by other roles cannot be enabled until the database administrator runs one of these:\n{grants}");
            }
            println!(
                "Organization production use requires postvec Pro; personal noncommercial use, non-production use and one 30-day production evaluation per organization are free. https://github.com/univec-ai/postvec/blob/main/LICENSING.md"
            );
        }
        Command::Status(_) => {
            if installed.is_none() {
                bail!("managed schema is not installed");
            }
            let status: String = sqlx::query_scalar("SELECT jsonb_build_object(
                'schema_version', (SELECT version FROM postvec.schema_version),
                'platform', (SELECT value FROM postvec.settings WHERE key = 'platform'),
                'worker_alive', COALESCE((SELECT last_beat > now() - interval '30 seconds' FROM postvec.worker_heartbeat), false),
                'heartbeat', (SELECT to_jsonb(h) FROM postvec.worker_heartbeat h),
                'leader', (SELECT value FROM postvec.settings WHERE key='leader'),
                'queue_depth', (SELECT count(*) FROM postvec.jobs),
                'dead_letters', (SELECT count(*) FROM postvec.jobs_dead))::text")
                .fetch_one(&mut *tx).await?;
            tx.commit().await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(&status)?)?
            );
        }
        Command::Uninstall(_) => {
            if installed.is_none() {
                bail!("managed schema is not installed");
            }
            let triggers: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT n.nspname::text, c.relname::text, t.tgname::text
                   FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                   JOIN pg_catalog.pg_proc p ON p.oid = t.tgfoid
                   JOIN pg_catalog.pg_namespace pn ON pn.oid = p.pronamespace
                  WHERE pn.nspname = 'postvec' AND NOT t.tgisinternal ORDER BY c.oid, t.oid",
            )
            .fetch_all(&mut *tx)
            .await?;
            for (schema, table, trigger) in triggers {
                tx.execute(
                    format!(
                        "DROP TRIGGER {} ON {}.{}",
                        quote_ident(&trigger),
                        quote_ident(&schema),
                        quote_ident(&table)
                    )
                    .as_str(),
                )
                .await?;
            }
            tx.execute(
                "DROP FUNCTION postvec._route(text, text); DROP VIEW postvec.routes;
                DROP TABLE postvec.lexical_df, postvec.lexical_stats, postvec.migrations, postvec.jobs_dead, postvec.jobs,
                postvec.registry, postvec.worker_heartbeat, postvec.models, postvec.settings,
                postvec.schema_version RESTRICT",
            )
            .await?;
            let functions: Vec<String> = sqlx::query_scalar(
                "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
                 JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'postvec'"
            ).fetch_all(&mut *tx).await?;
            for function in functions {
                tx.execute(format!("DROP FUNCTION {function} RESTRICT").as_str())
                    .await?;
            }
            tx.execute("DROP SCHEMA postvec RESTRICT").await?;
            tx.commit().await?;
            println!("Managed schema removed. User tables and vector columns retained.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn password_files_fail_closed_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("password");
        std::fs::write(&path, "private").unwrap();
        let mut args = ConnectionArgs {
            dsn: "postgresql://localhost/test".into(),
            password_file: Some(path.clone()),
            timeout: 1,
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(connect(&args)
            .await
            .unwrap_err()
            .to_string()
            .contains("0600"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&path, "first\nsecond").unwrap();
        assert!(connect(&args)
            .await
            .unwrap_err()
            .to_string()
            .contains("one nonempty password"));
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(path, &link).unwrap();
        args.password_file = Some(link);
        assert!(connect(&args)
            .await
            .unwrap_err()
            .to_string()
            .contains("cannot open password file"));
    }

    #[test]
    fn known_transaction_poolers_are_refused() {
        let pooled = |dsn: &str| transaction_pooler(&dsn.parse().unwrap());
        assert!(pooled(
            "postgresql://w.ref@aws-1-eu-west-1.pooler.supabase.com:6543/postgres"
        ));
        assert!(pooled("postgresql://w@db.ref.supabase.co:6543/postgres"));
        assert!(pooled(
            "postgresql://w@ep-cool-name-a1b2-pooler.eu-west-2.aws.neon.tech/neondb"
        ));
        assert!(!pooled(
            "postgresql://w.ref@aws-1-eu-west-1.pooler.supabase.com:5432/postgres"
        ));
        assert!(!pooled(
            "postgresql://w@ep-cool-name-a1b2.eu-west-2.aws.neon.tech/neondb"
        ));
    }

    #[test]
    fn dsn_password_detection_is_case_insensitive() {
        assert!(dsn_has_password("postgresql://u:secret@localhost/db"));
        assert!(dsn_has_password("host=localhost Password=secret user=u"));
        assert!(!dsn_has_password("postgresql://u@localhost/db"));
    }
}
