//! `postvec.migrate()`: switch a column's embedding model and convert the
//! existing vectors in place via ninference `ConvertEmbeddings`. No
//! re-embedding of source text. Build-new-column-then-swap, never in-place
//! `ALTER COLUMN TYPE` (that would rewrite the table under
//! `ACCESS EXCLUSIVE`).
//!
//! 1. `migrate()` pre-flights the convert path (direct -> two-hop bridge
//!    -> `reembed` fallback per strategy), adds `<vec>_new vector(M)`,
//!    inserts a `postvec.migrations` row and flips the registry to
//!    `state='migrating'`.
//! 2. The background worker's migration driver ([`crate::worker::migrate`])
//!    batch-converts rows `WHERE <new> IS NULL`, watermark-ordered. Fresh
//!    writes are routed to the new model/column by the job engine
//!    ([`crate::jobs::embed_routing`]) so they always win.
//! 3. `migration_finalize()` swaps: drop old column, rename new -> old
//!    name, update the registry. `reindex => 'blocking'` rebuilds the
//!    vector index inline; `'manual'` (default) leaves state
//!    `awaiting_index` and `migration_status()` carries the exact
//!    `CREATE INDEX CONCURRENTLY`.
//! 4. `migration_abort()` drops the new column and reverts. The old
//!    column is never touched before the swap.

use crate::api::embed::{resolve_convert, ConvertResolution};
use crate::api::registry::{
    assert_owner, build_vector_index, resolve_dim, resolve_relation, validate_choice,
};
use crate::registry::{
    distance_opclass, halfvec_opclass, quote_ident, quote_literal,
    vector_index_exists as registry_vector_index_exists, RegistryEntry,
};
use pgrx::prelude::*;
use pgrx::JsonB;

/// One row of `postvec.migrations`.
#[derive(Debug, Clone)]
pub struct Migration {
    pub id: i64,
    pub registry_id: i64,
    pub old_model: String,
    pub new_model: String,
    pub old_dim: i32,
    pub new_dim: i32,
    pub strategy: String,
    pub resolved_via: serde_json::Value,
    pub new_column: String,
    pub reindex: String,
    pub state: String,
    pub rows_total: i64,
    pub rows_done: i64,
    pub rows_skipped: i64,
    pub last_pk: Option<String>,
}

impl Migration {
    const SELECT: &'static str = "SELECT id, registry_id, old_model, new_model, old_dim, new_dim,
                strategy, resolved_via, new_column, reindex, state,
                rows_total, rows_done, rows_skipped, last_pk
           FROM postvec.migrations";

    fn from_row(row: &pgrx::spi::SpiHeapTupleData) -> Self {
        Migration {
            id: row.get::<i64>(1).unwrap().unwrap(),
            registry_id: row.get::<i64>(2).unwrap().unwrap(),
            old_model: row.get::<String>(3).unwrap().unwrap(),
            new_model: row.get::<String>(4).unwrap().unwrap(),
            old_dim: row.get::<i32>(5).unwrap().unwrap(),
            new_dim: row.get::<i32>(6).unwrap().unwrap(),
            strategy: row.get::<String>(7).unwrap().unwrap(),
            resolved_via: row
                .get::<JsonB>(8)
                .unwrap()
                .map(|j| j.0)
                .unwrap_or(serde_json::Value::Null),
            new_column: row.get::<String>(9).unwrap().unwrap(),
            reindex: row.get::<String>(10).unwrap().unwrap(),
            state: row.get::<String>(11).unwrap().unwrap(),
            rows_total: row.get::<i64>(12).unwrap().unwrap(),
            rows_done: row.get::<i64>(13).unwrap().unwrap(),
            rows_skipped: row.get::<i64>(14).unwrap().unwrap(),
            last_pk: row.get::<String>(15).unwrap(),
        }
    }

    /// Load by id. Must be called inside a transaction.
    pub fn load(id: i64) -> Option<Migration> {
        let q = format!("{} WHERE id = $1", Self::SELECT);
        Spi::connect(|client| {
            let t = client.select(q.as_str(), Some(1), &[id.into()]).ok()?;
            t.into_iter().next().map(|row| Migration::from_row(&row))
        })
    }

    /// Ids of all migrations the driver should be working on *now*: running,
    /// and past their `not_before` retry gate (pushed out exponentially by
    /// `set_migration_error` after a transient batch failure).
    pub fn running_ids() -> Vec<i64> {
        Spi::connect(|client| {
            let t = client
                .select(
                    "SELECT id FROM postvec.migrations
                      WHERE state = 'running' AND not_before <= now()
                      ORDER BY id",
                    None,
                    &[],
                )
                .expect("postvec: running_ids query failed");
            t.into_iter()
                .map(|r| r.get::<i64>(1).unwrap().unwrap())
                .collect()
        })
    }
}

/// The `CREATE INDEX CONCURRENTLY` statement an operator should run after a
/// finalized migration (dims > 2000 get the halfvec expression form, since
/// plain HNSW `vector_*_ops` indexes cap at 2000 dimensions).
pub(crate) fn suggested_index_sql(entry: &RegistryEntry) -> String {
    let idx = quote_ident(&format!("postvec_vec_{}", entry.id));
    let tbl = entry.qualified_vector_table();
    let col = quote_ident(&entry.vector_column);
    if entry.dim > 2000 {
        let opclass = halfvec_opclass(&entry.distance);
        format!(
            "CREATE INDEX CONCURRENTLY {idx} ON {tbl} USING hnsw \
             (({col}::halfvec({dim})) {opclass});",
            dim = entry.dim
        )
    } else {
        format!(
            "CREATE INDEX CONCURRENTLY {idx} ON {tbl} USING hnsw ({col} {opclass});",
            opclass = distance_opclass(&entry.distance)
        )
    }
}

/// Whether any index covers the entry's vector column (post-swap check).
fn vector_index_exists(entry: &RegistryEntry) -> bool {
    registry_vector_index_exists(&entry.qualified_vector_table(), &entry.vector_column)
}

/// What a `DROP COLUMN`-based swap would silently lose or trip over.
/// Read-only, resolved from the live catalog on every call. Never
/// persisted: a snapshot goes stale by finalize.
pub(crate) struct ColumnSwapHazards {
    /// Column-local semantics DROP COLUMN silently destroys: NOT NULL,
    /// generated/identity state, defaults, constraints, comments, column
    /// ACLs, security labels, statistics/storage settings, extended stats.
    pub(crate) lossy_metadata: Vec<String>,
    /// Normal external dependents (views, matviews, rules, incoming foreign
    /// keys, ...) that make a plain DROP COLUMN fail outright.
    pub(crate) blocking_dependents: Vec<String>,
    /// Indexes touching the column — expected cutover losses (informational;
    /// only the configured postvec ANN index is ever rebuilt by `reindex`).
    pub(crate) dependent_indexes: Vec<String>,
}

impl ColumnSwapHazards {
    pub(crate) fn is_clean_for_finalize(&self) -> bool {
        self.lossy_metadata.is_empty() && self.blocking_dependents.is_empty()
    }
}

/// Inspect the column across the whole partition tree. `DROP COLUMN` on a
/// partitioned parent recurses into every partition, so metadata or
/// dependents on a *child's* attribute are hazards of the swap exactly as the
/// parent's are — inspecting only the parent would silently lose them.
/// `pg_partition_tree` returns a plain table as its own single-node tree, so
/// the non-partitioned path is unchanged. Child diagnostics are prefixed with
/// the partition's name; output is sorted and deduplicated for stable tests.
pub(crate) fn inspect_column_swap(rel_oid: pg_sys::Oid, column: &str) -> ColumnSwapHazards {
    let mut rels: Vec<(pg_sys::Oid, String)> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT p.relid::oid AS rel_oid, p.relid::regclass::text AS rel_name
                   FROM pg_partition_tree($1) p ORDER BY p.relid",
                None,
                &[rel_oid.into()],
            )
            .unwrap();
        t.into_iter()
            .map(|r| {
                (
                    r.get::<pg_sys::Oid>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap().unwrap(),
                )
            })
            .collect()
    });
    if rels.is_empty() {
        // Defensive: inspect at least the relation itself.
        rels.push((rel_oid, String::new()));
    }
    let mut lossy = Vec::new();
    let mut blocking = Vec::new();
    let mut dependent_indexes = Vec::new();
    for (oid, name) in rels {
        let h = inspect_column_swap_relation(oid, column);
        let prefix = if oid == rel_oid {
            String::new()
        } else {
            format!("on partition {name}: ")
        };
        lossy.extend(h.lossy_metadata.into_iter().map(|s| format!("{prefix}{s}")));
        blocking.extend(
            h.blocking_dependents
                .into_iter()
                .map(|s| format!("{prefix}{s}")),
        );
        dependent_indexes.extend(
            h.dependent_indexes
                .into_iter()
                .map(|s| format!("{prefix}{s}")),
        );
    }
    lossy.sort();
    lossy.dedup();
    blocking.sort();
    blocking.dedup();
    dependent_indexes.sort();
    dependent_indexes.dedup();
    ColumnSwapHazards {
        lossy_metadata: lossy,
        blocking_dependents: blocking,
        dependent_indexes,
    }
}

/// Inspect the exact live attribute (`attnum` resolved from the relation OID
/// and column name) on **one** relation for everything a column swap would
/// lose or collide with. Catalog OIDs and per-column `pg_depend` edges only —
/// never SQL-text or expression substring matching, so a sibling `<col>_new`
/// column can never false-positive.
fn inspect_column_swap_relation(rel_oid: pg_sys::Oid, column: &str) -> ColumnSwapHazards {
    let mut lossy: Vec<String> = Vec::new();

    // pg_attribute facts (attstattarget: -1 on PG16, NULL on 17+ mean default):
    // (attnum, not_null, generated, identity, has_acl, custom_stats,
    //  custom_storage, compression, has_options).
    type AttSwapFacts = (i16, bool, String, String, bool, bool, bool, String, bool);
    let att: Option<AttSwapFacts> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT a.attnum,
                            a.attnotnull,
                            a.attgenerated::text,
                            a.attidentity::text,
                            a.attacl IS NOT NULL,
                            (a.attstattarget IS NOT NULL AND a.attstattarget <> -1),
                            a.attstorage <> t.typstorage,
                            a.attcompression::text,
                            a.attoptions IS NOT NULL
                       FROM pg_attribute a
                       JOIN pg_type t ON t.oid = a.atttypid
                      WHERE a.attrelid = $1 AND a.attname = $2
                        AND a.attnum > 0 AND NOT a.attisdropped",
                Some(1),
                &[rel_oid.into(), column.into()],
            )
            .unwrap();
        t.into_iter().next().map(|r| {
            (
                r.get::<i16>(1).unwrap().unwrap(),
                r.get::<bool>(2).unwrap().unwrap(),
                r.get::<String>(3).unwrap().unwrap_or_default(),
                r.get::<String>(4).unwrap().unwrap_or_default(),
                r.get::<bool>(5).unwrap().unwrap_or(false),
                r.get::<bool>(6).unwrap().unwrap_or(false),
                r.get::<bool>(7).unwrap().unwrap_or(false),
                r.get::<String>(8).unwrap().unwrap_or_default(),
                r.get::<bool>(9).unwrap().unwrap_or(false),
            )
        })
    });
    let Some((
        attnum,
        not_null,
        generated,
        identity,
        has_acl,
        custom_stats,
        custom_storage,
        compression,
        has_options,
    )) = att
    else {
        error!("postvec: column {column:?} vanished during swap inspection");
    };
    if not_null {
        lossy.push(format!("NOT NULL constraint on {column:?}"));
    }
    if !generated.is_empty() || !identity.is_empty() {
        lossy.push(format!("generated/identity definition on {column:?}"));
    }
    if has_acl {
        lossy.push(format!("column privileges (ACL) on {column:?}"));
    }
    if custom_stats {
        lossy.push(format!("custom statistics target on {column:?}"));
    }
    if custom_storage {
        lossy.push(format!("non-default storage setting on {column:?}"));
    }
    if !compression.is_empty() {
        lossy.push(format!(
            "explicit compression setting ({compression}) on {column:?}"
        ));
    }
    if has_options {
        lossy.push(format!("per-column options (attoptions) on {column:?}"));
    }

    let attnum = attnum as i32;
    let collect = |q: &str, prefix: &str, out: &mut Vec<String>| {
        let rows: Vec<String> = Spi::connect(|c| {
            let t = c.select(q, None, &[rel_oid.into(), attnum.into()]).unwrap();
            t.into_iter()
                .filter_map(|r| r.get::<String>(1).ok().flatten())
                .collect()
        });
        for r in rows {
            out.push(if prefix.is_empty() {
                r
            } else {
                format!("{prefix}{r}")
            });
        }
    };

    collect(
        "SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid)
           FROM pg_attrdef d WHERE d.adrelid = $1 AND d.adnum = $2",
        "column default: ",
        &mut lossy,
    );
    collect(
        "SELECT pg_catalog.quote_ident(c.conname) || ': ' ||
                pg_catalog.pg_get_constraintdef(c.oid)
           FROM pg_constraint c
          WHERE c.conrelid = $1 AND $2 = ANY(c.conkey)
          ORDER BY c.conname",
        "constraint ",
        &mut lossy,
    );
    collect(
        "SELECT pg_catalog.col_description($1, $2)",
        "column comment: ",
        &mut lossy,
    );
    collect(
        "SELECT 'security label (' || s.provider || '): ' || s.label
           FROM pg_seclabel s
          WHERE s.objoid = $1 AND s.classoid = 'pg_class'::regclass AND s.objsubid = $2",
        "",
        &mut lossy,
    );
    collect(
        "SELECT pg_catalog.quote_ident(n.nspname) || '.' ||
                pg_catalog.quote_ident(x.stxname)
           FROM pg_statistic_ext x
           JOIN pg_namespace n ON n.oid = x.stxnamespace
          WHERE x.stxrelid = $1 AND $2 = ANY(x.stxkeys::int2[])",
        "extended statistics ",
        &mut lossy,
    );

    // Indexes: per-column pg_depend edges (direct-key and expression indexes
    // both record them) — expected losses at cutover, reported by name.
    let mut dependent_indexes = Vec::new();
    collect(
        "SELECT DISTINCT pg_catalog.quote_ident(n.nspname) || '.' ||
                pg_catalog.quote_ident(ic.relname)
           FROM pg_depend d
           JOIN pg_class ic ON ic.oid = d.objid AND ic.relkind IN ('i', 'I')
           JOIN pg_namespace n ON n.oid = ic.relnamespace
          WHERE d.classid = 'pg_class'::regclass
            AND d.refclassid = 'pg_class'::regclass
            AND d.refobjid = $1 AND d.refobjsubid = $2",
        "",
        &mut dependent_indexes,
    );

    // Normal external dependents: exactly what makes DROP COLUMN (without
    // CASCADE) fail. View-like objects are named as the operator knows them
    // via pg_rewrite.ev_class; pg_describe_object is the fallback for every
    // other object class.
    let mut blocking = Vec::new();
    collect(
        "SELECT DISTINCT
                CASE WHEN d.classid = 'pg_rewrite'::regclass THEN
                    (SELECT pg_catalog.quote_ident(vn.nspname) || '.' ||
                            pg_catalog.quote_ident(vc.relname) ||
                            CASE vc.relkind WHEN 'm' THEN ' (materialized view)'
                                            WHEN 'v' THEN ' (view)'
                                            ELSE ' (rule)' END
                       FROM pg_rewrite rw
                       JOIN pg_class vc ON vc.oid = rw.ev_class
                       JOIN pg_namespace vn ON vn.oid = vc.relnamespace
                      WHERE rw.oid = d.objid)
                ELSE pg_catalog.pg_describe_object(d.classid, d.objid, d.objsubid)
                END
           FROM pg_depend d
          WHERE d.refclassid = 'pg_class'::regclass
            AND d.refobjid = $1 AND d.refobjsubid = $2
            AND d.deptype = 'n'",
        "",
        &mut blocking,
    );

    lossy.sort();
    lossy.dedup();
    blocking.sort();
    blocking.dedup();
    dependent_indexes.sort();
    dependent_indexes.dedup();
    ColumnSwapHazards {
        lossy_metadata: lossy,
        blocking_dependents: blocking,
        dependent_indexes,
    }
}

fn bulleted(items: &[String]) -> String {
    items
        .iter()
        .map(|i| format!("  - {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn load_entry_checked(relation: &str, column_name: &str) -> RegistryEntry {
    let rel = resolve_relation(relation);
    assert_owner(&rel);
    RegistryEntry::load_active(&rel.schema, &rel.table, column_name).unwrap_or_else(|| {
        error!(
            "postvec: {}.{}.{column_name} is not enabled",
            rel.schema, rel.table
        )
    })
}

#[pg_extern]
fn migrate(
    relation: &str,
    column_name: &str,
    new_model: &str,
    strategy: default!(&str, "'convert'"),
    reindex: default!(&str, "'manual'"),
    observed_writes_quiesced: default!(bool, false),
) -> i64 {
    crate::api::registry::apply_ddl_lock_timeout();
    let strategy = validate_choice("strategy", strategy, &["convert", "reembed", "auto"]);
    let reindex = validate_choice("reindex", reindex, &["manual", "blocking"]);

    let entry = load_entry_checked(relation, column_name);
    if entry.state != "active" {
        error!(
            "postvec: {}.{}.{column_name} is in state {:?}; only active entries can migrate \
             (finalize or abort the running migration first)",
            entry.table_schema, entry.table_name, entry.state
        );
    }
    if new_model == entry.model {
        error!("postvec: {new_model:?} is already the model for this column");
    }
    // Observed entries (adopt(sync => false)) have no DML triggers, so the
    // online-migration guarantee does not hold: an application write can
    // escape the migration watermark and be silently lost or left stale.
    // The lean production contract is offline migration: the operator
    // stops application writes before migrate() and keeps them stopped
    // until migration_finalize() commits, acknowledged explicitly.
    // Synced entries ignore the flag.
    if entry.trigger_mode == "none" && !observed_writes_quiesced {
        error!(
            "postvec: {}.{}.{column_name} is an observed entry (no DML triggers): application \
             writes during migration can escape the migration watermark and postvec cannot \
             see them. Stop application writes to the table, keep them stopped until \
             postvec.migration_finalize() commits, and acknowledge with:\n  \
             SELECT postvec.migrate({rel_lit}, {col_lit}, new_model => \
             {model_lit}, observed_writes_quiesced => true);",
            entry.table_schema,
            entry.table_name,
            rel_lit = quote_literal(relation),
            col_lit = quote_literal(column_name),
            model_lit = quote_literal(new_model),
        );
    }

    let new_dim = resolve_dim(new_model);
    if reindex == "blocking" && new_dim > 2000 {
        error!(
            "postvec: reindex => 'blocking' cannot build an HNSW index over {new_dim} dims \
             (pgvector caps vector_*_ops at 2000); use reindex => 'manual' and a halfvec \
             expression index"
        );
    }

    // Pre-flight the convert path: direct convert model -> two-hop
    // bridge -> reembed fallback, per strategy. This is what prevents
    // a late BridgePathNotFound after millions of rows are in flight.
    let resolved_via: serde_json::Value = match strategy.as_str() {
        "reembed" => serde_json::json!({ "kind": "reembed" }),
        _ => match resolve_convert(&entry.model, new_model) {
            Ok(ConvertResolution::Direct(name)) => {
                serde_json::json!({ "kind": "direct", "model": name })
            }
            Ok(ConvertResolution::Bridge { executor, bridge }) => {
                serde_json::json!({ "kind": "bridge", "executor": executor, "bridge": bridge })
            }
            Err(e) => {
                if strategy == "auto" {
                    serde_json::json!({ "kind": "reembed" })
                } else {
                    error!(
                        "postvec: {e}; rerun with strategy => 'reembed' to re-embed source \
                         text, or 'auto' to prefer convert and fall back"
                    );
                }
            }
        },
    };
    let is_reembed = resolved_via["kind"] == "reembed";

    let new_column = format!("{}_new", entry.vector_column);
    if new_column.len() > 63 {
        // NAMEDATALEN: Postgres would silently truncate the identifier and
        // desync it from the registry bookkeeping.
        error!(
            "postvec: migration column name {new_column:?} exceeds 63 bytes; rename the \
             vector column (disable/enable with a shorter vector_column) first"
        );
    }
    // The scratch column and the whole swap target the entry's vector
    // table: the managed destination for a recursive entry, whose chunk
    // vectors are what migrate.
    let qtable = entry.qualified_vector_table();
    let rel = resolve_relation(&entry.qualified_vector_table());
    if crate::api::registry::column_type(rel.oid, &new_column).is_some() {
        error!(
            "postvec: column {new_column:?} already exists on {qtable} — a previous migration \
             left it behind; postvec.migration_abort() it or drop the column"
        );
    }

    // Column-swap preflight, before any DDL or state change. Lossy
    // column-local metadata refuses outright (the swap would silently
    // strip it). External dependents and indexes warn now and, for the
    // dependents, gate finalize. Especially relevant to adopted columns,
    // which carry application semantics postvec never created. On a
    // recursive destination the generated join view is deliberately not
    // a dependent (it omits the vector column), so a clean chunked entry
    // sails through. User-created views on the destination gate finalize
    // exactly like column mode.
    let hazards = inspect_column_swap(rel.oid, &entry.vector_column);
    if !hazards.lossy_metadata.is_empty() {
        error!(
            "postvec: {vec:?} on {qtable} carries column-local metadata that the migration \
             column swap would silently drop:\n{items}\nSave each object's DDL, remove it, \
             rerun migrate(), and recreate it on the replacement column after \
             postvec.migration_finalize()",
            vec = entry.vector_column,
            items = bulleted(&hazards.lossy_metadata),
        );
    }
    if !hazards.blocking_dependents.is_empty() {
        warning!(
            "postvec: these objects depend on {vec:?} and will block \
             postvec.migration_finalize():\n{items}\nThey may stay usable while the \
             conversion runs, but must be dropped or reworked immediately before cutover",
            vec = entry.vector_column,
            items = bulleted(&hazards.blocking_dependents),
        );
    }
    if !hazards.dependent_indexes.is_empty() {
        warning!(
            "postvec: every index on {vec:?} is dropped at a successful cutover:\n{items}\n\
             Only the configured postvec ANN index is rebuilt per `reindex`; all other \
             indexes are yours to recreate on the replacement column",
            vec = entry.vector_column,
            items = bulleted(&hazards.dependent_indexes),
        );
    }

    // Progress denominators (chunk counts for a recursive entry), computed
    // BEFORE the ADD COLUMN so the full-table scan runs under an ordinary
    // ACCESS SHARE lock rather than the ACCESS EXCLUSIVE the ADD COLUMN
    // takes. The counts are advisory; rows changing between the count and
    // the first batch only shift the displayed percentage. For column-mode
    // reembed, rows whose text is gone can never be re-embedded, so count
    // them up front as skipped (they keep NULL), in the same scan as the
    // total (`count(*) FILTER`). A chunk's text is NOT NULL by schema, so
    // recursive reembed has no such precount; chunks whose source row
    // vanishes mid-migration are purged by the triggers.
    let old_vec = quote_ident(&entry.vector_column);
    let (rows_total, rows_skipped) = if is_reembed {
        if entry.is_recursive() {
            (
                Spi::get_one::<i64>(&format!("SELECT count(*) FROM {qtable}"))
                    .unwrap()
                    .unwrap_or(0),
                0,
            )
        } else {
            let src = quote_ident(&entry.source_column);
            Spi::connect(|c| {
                let t = c
                    .select(
                        &format!(
                            "SELECT count(*) FILTER (WHERE {src} IS NOT NULL),
                                    count(*) FILTER (WHERE {old_vec} IS NOT NULL
                                                       AND {src} IS NULL)
                               FROM {qtable}"
                        ),
                        Some(1),
                        &[],
                    )
                    .unwrap_or_else(|e| error!("postvec: migration precount failed: {e}"));
                let row = t.into_iter().next();
                let row = row.as_ref();
                (
                    row.and_then(|r| r.get::<i64>(1).unwrap()).unwrap_or(0),
                    row.and_then(|r| r.get::<i64>(2).unwrap()).unwrap_or(0),
                )
            })
        }
    } else {
        (
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM {qtable} WHERE {old_vec} IS NOT NULL"
            ))
            .unwrap()
            .unwrap_or(0),
            0,
        )
    };

    Spi::run(&format!(
        "ALTER TABLE {qtable} ADD COLUMN {col} vector({new_dim})",
        col = quote_ident(&new_column),
    ))
    .unwrap_or_else(|e| error!("postvec: ADD COLUMN for migration failed: {e}"));

    let mid = Spi::get_one_with_args::<i64>(
        "INSERT INTO postvec.migrations
             (registry_id, old_model, new_model, old_dim, new_dim, strategy,
              resolved_via, new_column, reindex, rows_total, rows_skipped)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)
         RETURNING id",
        &[
            entry.id.into(),
            entry.model.as_str().into(),
            new_model.into(),
            entry.dim.into(),
            new_dim.into(),
            strategy.as_str().into(),
            JsonB(resolved_via.clone()).into(),
            new_column.as_str().into(),
            reindex.as_str().into(),
            rows_total.into(),
            rows_skipped.into(),
        ],
    )
    .unwrap()
    .expect("migrations insert returned no id");

    Spi::run_with_args(
        "UPDATE postvec.registry SET state = 'migrating' WHERE id = $1",
        &[entry.id.into()],
    )
    .unwrap();
    crate::worker::worker_kick();

    log!(
        "postvec: migration {mid} started for {qtable}.{col}: {old} -> {new} \
         (strategy={strategy}, via={resolved_via}, {rows_total} rows)",
        col = entry.source_column,
        old = entry.model,
        new = new_model,
    );
    mid
}

/// Finalize a completed migration: swap columns, update the registry, and
/// handle the index per the migration's `reindex` mode. Call again on an
/// `awaiting_index` migration after building the index to mark it done.
#[pg_extern]
fn migration_finalize(migration_id: i64) {
    crate::api::registry::apply_ddl_lock_timeout();
    let m = Migration::load(migration_id)
        .unwrap_or_else(|| error!("postvec: migration {migration_id} does not exist"));
    let entry = RegistryEntry::load(m.registry_id)
        .unwrap_or_else(|| error!("postvec: registry entry {} is gone", m.registry_id));
    let rel = resolve_relation(&entry.qualified_table());
    assert_owner(&rel);

    match m.state.as_str() {
        "running" => error!(
            "postvec: migration {migration_id} is still converting ({}/{} rows); wait for \
             state 'awaiting_finalize' (postvec.migration_status())",
            m.rows_done, m.rows_total
        ),
        "awaiting_finalize" => {}
        "awaiting_index" => {
            // Second call: the operator built the index (or decided not to).
            // Readiness means an index `search()` will actually use: a valid/
            // ready/live ANN index on the vector column whose opclass matches
            // the entry's distance and dimension — any-ANN would let a
            // wrong-opclass index (which search() ignores) mark the
            // migration done.
            //
            // Deliberately NO extension stamping here: the index was built
            // outside postvec (the suggested CREATE INDEX CONCURRENTLY),
            // so it is operator-owned — only indexes postvec itself creates
            // carry the extension dependency. Claiming an externally-built
            // object by its conventional name risked stamping a *different*
            // index than the one detected as usable, turning a user object
            // into DROP EXTENSION collateral. Teardown consequently leaves
            // such an index in place (with a WARNING), which is the correct
            // side of the ownership invariant.
            let entry = RegistryEntry::load(m.registry_id).unwrap();
            // The shared readiness predicate: one valid/ready/live ANN
            // index with the expected opclass — ownership-neutral.
            let ready = crate::api::registry::ann_index_ready(&entry);
            if ready {
                Spi::run_with_args(
                    "UPDATE postvec.migrations
                        SET state = 'done', finished_at = now() WHERE id = $1",
                    &[migration_id.into()],
                )
                .unwrap();
                log!("postvec: migration {migration_id} done (index present)");
            } else {
                let expected = crate::registry::expected_ann_opclass(&entry.distance, entry.dim);
                warning!(
                    "postvec: migration {migration_id} still awaits a valid ANN index using \
                     {expected} on {vec:?}; run:\n{}",
                    suggested_index_sql(&entry),
                    vec = entry.vector_column,
                );
            }
            return;
        }
        other => error!("postvec: migration {migration_id} is {other:?}; nothing to finalize"),
    }

    let pending = Spi::get_one_with_args::<i64>(
        "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1",
        &[entry.id.into()],
    )
    .unwrap()
    .unwrap_or(0);
    if pending > 0 {
        // Safe either way: the job engine re-reads the routing inside its
        // write-back transaction, so post-swap write-backs land correctly.
        log!("postvec: finalizing with {pending} jobs still pending (they re-route safely)");
    }

    // The swap happens on the vector table: the destination for a
    // recursive entry. Lock order is source -> destination (-> registry
    // row), matching every other lifecycle path. For column mode the two
    // are the same relation and one lock suffices.
    let qtable = entry.qualified_vector_table();
    let old_col = quote_ident(&entry.vector_column);
    let new_col = quote_ident(&m.new_column);

    // The check-to-DDL race is closed with an explicit lock: no concurrent
    // transaction can add a dependent or mutate column metadata between
    // the clean inspection below and the swap. The lock is held to commit;
    // DROP COLUMN would take it anyway.
    if entry.is_recursive() {
        Spi::run(&format!(
            "LOCK TABLE {} IN ACCESS EXCLUSIVE MODE",
            entry.qualified_table()
        ))
        .unwrap_or_else(|e| error!("postvec: locking the source for finalize failed: {e}"));
    }
    Spi::run(&format!("LOCK TABLE {qtable} IN ACCESS EXCLUSIVE MODE"))
        .unwrap_or_else(|e| error!("postvec: locking {qtable} for finalize failed: {e}"));
    let target_rel = resolve_relation(&entry.qualified_vector_table());
    let hazards = inspect_column_swap(target_rel.oid, &entry.vector_column);
    if !hazards.is_clean_for_finalize() {
        // Neither DROP nor RENAME has run: the migration stays
        // 'awaiting_finalize' with both columns intact, and finalize can be
        // retried after remediation.
        let mut items = Vec::new();
        if !hazards.lossy_metadata.is_empty() {
            items.push(format!(
                "column-local metadata the swap would silently drop (save its DDL, remove \
                 it, recreate it on the replacement column afterwards):\n{}",
                bulleted(&hazards.lossy_metadata)
            ));
        }
        if !hazards.blocking_dependents.is_empty() {
            items.push(format!(
                "objects that depend on {vec:?} and block DROP COLUMN (drop or rework them \
                 first; postvec never uses CASCADE):\n{}",
                bulleted(&hazards.blocking_dependents),
                vec = entry.vector_column,
            ));
        }
        error!(
            "postvec: migration {migration_id} cannot finalize yet:\n{}\nThe migration \
             remains 'awaiting_finalize' with both columns intact; remediate and call \
             postvec.migration_finalize({migration_id}) again",
            items.join("\n")
        );
    }

    // Whether the old column carried an index (it dies with the column): if
    // it didn't, 'manual' mode has nothing to rebuild and finishes directly.
    let had_index = vector_index_exists(&entry);

    // The swap: one transaction (this function's). Dropping the old column
    // also drops any index on it. Never CASCADE.
    Spi::run(&format!("ALTER TABLE {qtable} DROP COLUMN {old_col}"))
        .unwrap_or_else(|e| error!("postvec: dropping the old vector column failed: {e}"));
    Spi::run(&format!(
        "ALTER TABLE {qtable} RENAME COLUMN {new_col} TO {old_col}"
    ))
    .unwrap_or_else(|e| error!("postvec: renaming the new vector column failed: {e}"));
    // After the swap the live column *is* postvec's (the old one was dropped,
    // the postvec-created `_new` renamed into its place), so ownership flips
    // in the same registry update that moves model/dim — without this, an
    // adopt-then-migrate user would keep a drop_column refusal forever on a
    // column postvec built. Trigger mode is deliberately untouched: an
    // observed entry stays observed until promoted.
    Spi::run_with_args(
        "UPDATE postvec.registry
            SET model = $1, dim = $2, state = 'active', owns_vector_column = true
          WHERE id = $3",
        &[
            m.new_model.as_str().into(),
            m.new_dim.into(),
            entry.id.into(),
        ],
    )
    .unwrap();

    let entry = RegistryEntry::load(entry.id).unwrap();
    match m.reindex.as_str() {
        "blocking" => {
            // The rebuilt index lives on the vector table: the destination
            // for a recursive entry. The source has no vector column there.
            build_vector_index(
                entry.id,
                &entry.qualified_vector_table(),
                &entry.vector_column,
                entry.dim,
                &entry.distance,
            );
            Spi::run_with_args(
                "UPDATE postvec.migrations SET state = 'done', finished_at = now() WHERE id = $1",
                &[migration_id.into()],
            )
            .unwrap();
            log!("postvec: migration {migration_id} finalized (index rebuilt inline)");
        }
        _ if had_index => {
            Spi::run_with_args(
                "UPDATE postvec.migrations SET state = 'awaiting_index' WHERE id = $1",
                &[migration_id.into()],
            )
            .unwrap();
            log!(
                "postvec: migration {migration_id} finalized; build the vector index, then call \
                 postvec.migration_finalize({migration_id}) again:\n{}",
                suggested_index_sql(&entry)
            );
        }
        _ => {
            Spi::run_with_args(
                "UPDATE postvec.migrations SET state = 'done', finished_at = now() WHERE id = $1",
                &[migration_id.into()],
            )
            .unwrap();
            log!("postvec: migration {migration_id} finalized (no vector index to rebuild)");
        }
    }
}

/// Abort a migration cleanly: the old column was never touched, so this just
/// drops the new column and reverts the registry.
#[pg_extern]
fn migration_abort(migration_id: i64) {
    crate::api::registry::apply_ddl_lock_timeout();
    let m = Migration::load(migration_id)
        .unwrap_or_else(|| error!("postvec: migration {migration_id} does not exist"));
    let entry = RegistryEntry::load(m.registry_id)
        .unwrap_or_else(|| error!("postvec: registry entry {} is gone", m.registry_id));
    let rel = resolve_relation(&entry.qualified_table());
    assert_owner(&rel);

    if !matches!(m.state.as_str(), "running" | "awaiting_finalize" | "failed") {
        error!(
            "postvec: migration {migration_id} is {:?}; only running/awaiting_finalize/failed \
             migrations can abort (the column swap already happened)",
            m.state
        );
    }

    Spi::run(&format!(
        "ALTER TABLE {tbl} DROP COLUMN IF EXISTS {col}",
        tbl = entry.qualified_vector_table(),
        col = quote_ident(&m.new_column),
    ))
    .unwrap_or_else(|e| error!("postvec: dropping the migration column failed: {e}"));
    Spi::run_with_args(
        "UPDATE postvec.registry SET state = 'active' WHERE id = $1",
        &[entry.id.into()],
    )
    .unwrap();
    Spi::run_with_args(
        "UPDATE postvec.migrations SET state = 'aborted', finished_at = now() WHERE id = $1",
        &[migration_id.into()],
    )
    .unwrap();
    log!(
        "postvec: migration {migration_id} aborted ({}.{} untouched)",
        entry.table_schema,
        entry.table_name
    );
}

#[pg_extern]
#[allow(clippy::type_complexity)]
fn migration_status(
    migration_id: default!(Option<i64>, "NULL"),
) -> TableIterator<
    'static,
    (
        name!(migration_id, i64),
        name!(registry_id, i64),
        name!(relation, String),
        name!(source_column, String),
        name!(old_model, String),
        name!(new_model, String),
        name!(strategy, String),
        name!(resolved_via, Option<String>),
        name!(state, String),
        name!(rows_total, i64),
        name!(rows_done, i64),
        name!(rows_skipped, i64),
        name!(progress_pct, Option<f64>),
        name!(error, Option<String>),
        name!(started_at, String),
        name!(finished_at, Option<String>),
        name!(suggested_index_sql, Option<String>),
    ),
> {
    let rows = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT m.id, m.registry_id,
                        r.table_schema || '.' || r.table_name,
                        r.source_column,
                        m.old_model, m.new_model, m.strategy,
                        m.resolved_via::text, m.state,
                        m.rows_total, m.rows_done, m.rows_skipped,
                        CASE WHEN m.rows_total > 0
                             -- capped: fresh writes routed to the new column
                             -- shrink the driver's share of rows_total
                             THEN LEAST(100.0, round(100.0 * (m.rows_done + m.rows_skipped)
                                        / m.rows_total, 1))::float8
                             ELSE NULL END,
                        m.error, m.started_at::text, m.finished_at::text,
                        m.state = 'awaiting_index' AS wants_index
                   FROM postvec.migrations m
                   JOIN postvec.registry r ON r.id = m.registry_id
                  WHERE $1::bigint IS NULL OR m.id = $1
                  ORDER BY m.id",
                None,
                &[migration_id.into()],
            )
            .expect("postvec: migration_status query failed");
        t.into_iter()
            .map(|r| {
                let registry_id = r.get::<i64>(2).unwrap().unwrap();
                let wants_index = r.get::<bool>(17).unwrap().unwrap_or(false);
                let index_sql = if wants_index {
                    RegistryEntry::load(registry_id).map(|e| suggested_index_sql(&e))
                } else {
                    None
                };
                (
                    r.get::<i64>(1).unwrap().unwrap(),
                    registry_id,
                    r.get::<String>(3).unwrap().unwrap(),
                    r.get::<String>(4).unwrap().unwrap(),
                    r.get::<String>(5).unwrap().unwrap(),
                    r.get::<String>(6).unwrap().unwrap(),
                    r.get::<String>(7).unwrap().unwrap(),
                    r.get::<String>(8).unwrap(),
                    r.get::<String>(9).unwrap().unwrap(),
                    r.get::<i64>(10).unwrap().unwrap(),
                    r.get::<i64>(11).unwrap().unwrap(),
                    r.get::<i64>(12).unwrap().unwrap(),
                    r.get::<f64>(13).unwrap(),
                    r.get::<String>(14).unwrap(),
                    r.get::<String>(15).unwrap().unwrap(),
                    r.get::<String>(16).unwrap(),
                    index_sql,
                )
            })
            .collect::<Vec<_>>()
    });
    TableIterator::new(rows)
}

/// Shared test fixtures (also used by the driver tests in
/// `crate::worker::migrate`).
#[cfg(any(test, feature = "pg_test"))]
pub(crate) mod test_fixtures {
    use pgrx::prelude::*;

    /// Cache fixture: embed models m (dim 3), m2 (dim 4), m3 (dim 5); a
    /// direct converter m→m2; no path at all to m3.
    pub(crate) fn seed_models() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('m',  'embed',   NULL, 'm',  3, '{}'::jsonb),
                    ('m2', 'embed',   NULL, 'm2', 4, '{}'::jsonb),
                    ('m3', 'embed',   NULL, 'm3', 5, '{}'::jsonb),
                    ('conv-m-m2', 'convert', 'm', 'm2', 4, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
    }

    /// docs table enabled on model m with three hand-filled 3-dim vectors and
    /// one row without a vector. Returns the registry id.
    pub(crate) fn docs_enabled() -> i64 {
        seed_models();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('bb'), ('ccc'), ('no vector yet')")
            .unwrap();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
            .unwrap()
            .unwrap();
        Spi::run(
            "UPDATE docs SET body_semantic =
                 ('[' || id || ',' || id || ',' || id || ']')::vector
              WHERE id <= 3",
        )
        .unwrap();
        id
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use super::test_fixtures::docs_enabled;
    use pgrx::prelude::*;

    #[pg_test]
    fn migrate_preflight_direct_convert() {
        let rid = docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();

        // Registry flips to migrating; the new column exists with the new dim.
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("migrating")
        );
        let typ = Spi::get_one::<String>(
            "SELECT pg_catalog.format_type(atttypid, atttypmod) FROM pg_attribute
              WHERE attrelid = 'docs'::regclass AND attname = 'body_semantic_new'",
        )
        .unwrap();
        assert_eq!(typ.as_deref(), Some("vector(4)"));

        // Pre-flight resolved the direct converter; totals count old vectors.
        let (kind, total) = (
            Spi::get_one_with_args::<String>(
                "SELECT resolved_via->>'kind' FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Spi::get_one_with_args::<i64>(
                "SELECT rows_total FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
        );
        assert_eq!(kind.as_deref(), Some("direct"));
        assert_eq!(total, Some(3), "three rows carry old vectors");
        let _ = rid;
    }

    #[pg_test]
    fn migrate_no_path_refused() {
        docs_enabled();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m3')").ok();
        });
        assert!(r.is_err(), "no convert path m->m3 must refuse");
    }

    #[pg_test]
    fn migrate_auto_falls_back_to_reembed() {
        docs_enabled();
        let mid =
            Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m3', strategy => 'auto')")
                .unwrap()
                .unwrap();
        let kind = Spi::get_one_with_args::<String>(
            "SELECT resolved_via->>'kind' FROM postvec.migrations WHERE id = $1",
            &[mid.into()],
        )
        .unwrap();
        assert_eq!(kind.as_deref(), Some("reembed"));
    }

    #[pg_test]
    fn migrate_reembed_precounts_unconvertible() {
        docs_enabled();
        // One row has a vector but its text was redacted: reembed can never
        // fill it; it is counted skipped up front.
        Spi::run("UPDATE docs SET body = NULL WHERE id = 1").unwrap();
        let mid = Spi::get_one::<i64>(
            "SELECT postvec.migrate('docs','body','m3', strategy => 'reembed')",
        )
        .unwrap()
        .unwrap();
        let (total, skipped) = (
            Spi::get_one_with_args::<i64>(
                "SELECT rows_total FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
            Spi::get_one_with_args::<i64>(
                "SELECT rows_skipped FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap(),
        );
        assert_eq!(total, Some(3), "three rows still have text");
        assert_eq!(skipped, Some(1), "the redacted row is pre-counted");
    }

    #[pg_test]
    fn second_migrate_refused_while_migrating() {
        docs_enabled();
        Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").ok();
        });
        assert!(r.is_err(), "one migration at a time");
    }

    #[pg_test]
    fn finalize_refused_while_running() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).ok();
        });
        assert!(r.is_err(), "cannot finalize a still-running migration");
    }

    #[pg_test]
    fn disable_refused_while_migrating() {
        docs_enabled();
        Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.disable('docs','body')").ok();
        });
        assert!(r.is_err(), "disable must refuse during a migration");
    }

    #[pg_test]
    fn abort_reverts_cleanly() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();

        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active")
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT model FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("m"),
            "old model kept"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass
                    AND attname = 'body_semantic_new' AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "migration column dropped"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()],
            )
            .unwrap()
            .as_deref(),
            Some("aborted")
        );
        // Old vectors untouched.
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs WHERE body_semantic IS NOT NULL")
                .unwrap(),
            Some(3)
        );
    }

    /// progress_pct is capped at 100 — fresh writes routed to the new column
    /// shrink the driver's share of the rows_total snapshot, so done+skipped
    /// bookkeeping can otherwise drift past the denominator.
    #[pg_test]
    fn migration_progress_pct_is_capped() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        Spi::run_with_args(
            "UPDATE postvec.migrations SET rows_done = rows_total + 5 WHERE id = $1",
            &[mid.into()],
        )
        .unwrap();
        let pct = Spi::get_one_with_args::<f64>(
            "SELECT progress_pct FROM postvec.migration_status($1)",
            &[mid.into()],
        )
        .unwrap();
        assert_eq!(pct, Some(100.0));
    }

    #[pg_test]
    fn migration_status_reports_progress() {
        docs_enabled();
        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')")
            .unwrap()
            .unwrap();
        let state = Spi::get_one_with_args::<String>(
            "SELECT state FROM postvec.migration_status($1)",
            &[mid.into()],
        )
        .unwrap();
        assert_eq!(state.as_deref(), Some("running"));
        let rel = Spi::get_one_with_args::<String>(
            "SELECT relation FROM postvec.migration_status($1)",
            &[mid.into()],
        )
        .unwrap();
        assert_eq!(rel.as_deref(), Some("public.docs"));
    }

    #[pg_test]
    fn high_dim_halfvec_expression_index_is_detected_for_finalize() {
        Spi::run(
            "INSERT INTO postvec.models
                 (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('m_hi',  'embed',   NULL,   'm_hi',  3001, '{}'::jsonb),
                    ('m2_hi', 'embed',   NULL,   'm2_hi', 3002, '{}'::jsonb),
                    ('conv-hi', 'convert', 'm_hi', 'm2_hi', 3002, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let rid =
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m_hi', backfill => false)")
                .unwrap()
                .unwrap();

        Spi::run(&format!(
            "CREATE INDEX postvec_vec_{rid}
                ON docs USING hnsw ((body_semantic::halfvec(3001)) halfvec_cosine_ops)"
        ))
        .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT has_vector_index FROM postvec.status()").unwrap(),
            Some(true),
            "status() sees the halfvec expression index"
        );

        let mid = Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2_hi')")
            .unwrap()
            .unwrap();
        assert!(matches!(
            crate::worker::migrate::read_migration_batch(mid, 64),
            crate::worker::migrate::BatchRead::Finished
        ));

        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()]
            )
            .unwrap()
            .as_deref(),
            Some("awaiting_index"),
            "manual reindex is required because the old expression index existed"
        );

        let suggested = Spi::get_one::<String>(&format!(
            "SELECT suggested_index_sql FROM postvec.migration_status({mid})"
        ))
        .unwrap()
        .unwrap();
        assert!(
            suggested.contains("halfvec(3002)"),
            "suggestion should be a halfvec expression index: {suggested}"
        );
        // pg_test runs inside a transaction; CONCURRENTLY is not allowed there.
        Spi::run(&suggested.replace("CONCURRENTLY ", "")).unwrap();
        Spi::run(&format!("SELECT postvec.migration_finalize({mid})")).unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.migrations WHERE id = $1",
                &[mid.into()]
            )
            .unwrap()
            .as_deref(),
            Some("done"),
            "second finalize sees the halfvec expression index"
        );
    }
}
