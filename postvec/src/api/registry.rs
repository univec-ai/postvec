//! `postvec.enable()` / `postvec.disable()`.
//!
//! `enable()` declares a text column semantic: it introspects the table,
//! sizes and adds the shadow `vector(N)` column, records the registry
//! entry, attaches the enqueue/TRUNCATE triggers, optionally builds the
//! FTS index and backfills existing rows. All dynamic DDL routes
//! identifiers through [`quote_ident`] / [`quote_literal`], never
//! string-concatenated raw. Both functions run in the caller's
//! transaction, so any `error!` rolls the whole operation back.

use crate::api::embed::embed_texts;
use crate::registry::RegistryEntryDb as _;
use crate::registry::{quote_ident, quote_literal, RegistryEntry};
use pgrx::prelude::*;

pub(crate) struct RelInfo {
    pub(crate) oid: pg_sys::Oid,
    pub(crate) schema: String,
    pub(crate) table: String,
    pub(crate) relkind: String,
    /// `relpersistence`: `p` permanent, `u` unlogged, `t` temporary.
    pub(crate) persistence: String,
    pub(crate) owner_ok: bool,
}

/// Resolve `'schema.table'` (or `'table'`) to its oid and catalog facts.
pub(crate) fn resolve_relation(relation: &str) -> RelInfo {
    let oid =
        Spi::get_one_with_args::<pg_sys::Oid>("SELECT to_regclass($1)::oid", &[relation.into()])
            .unwrap();
    let oid = match oid {
        Some(o) if o != pg_sys::Oid::INVALID => o,
        _ => error!("postvec: relation {relation:?} does not exist"),
    };
    Spi::connect(|c| {
        let t = c
            .select(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text,
                        c.relpersistence::text,
                        pg_catalog.pg_has_role(current_user, c.relowner, 'USAGE')
                   FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                  WHERE c.oid = $1",
                Some(1),
                &[oid.into()],
            )
            .unwrap();
        let r = t.into_iter().next().expect("relation vanished");
        RelInfo {
            oid,
            schema: r.get::<String>(1).unwrap().unwrap(),
            table: r.get::<String>(2).unwrap().unwrap(),
            relkind: r.get::<String>(3).unwrap().unwrap(),
            persistence: r.get::<String>(4).unwrap().unwrap(),
            owner_ok: r.get::<bool>(5).unwrap().unwrap(),
        }
    })
}

/// Run `f` under `SET LOCAL search_path TO pg_catalog`, restoring the
/// caller's setting afterwards. Catalog renderings taken through this helper
/// (`format_type` output in particular) spell non-`pg_catalog` types with
/// their schema qualification, so text stored in the registry and later
/// interpolated into **worker** SQL — which runs without the enabling
/// session's search path, always resolves.
/// Run `f` under `KEY_SETTINGS`, the format queued primary keys are written
/// and parsed in, restoring the caller's settings afterwards.
pub(crate) fn with_key_format<R>(f: impl FnOnce() -> R) -> R {
    let set = |name: &str, value: &str| {
        Spi::run_with_args(
            "SELECT pg_catalog.set_config($1, $2, true)",
            &[name.into(), value.into()],
        )
        .unwrap_or_else(|e| error!("postvec: setting {name} failed: {e}"))
    };
    let saved: Vec<(&str, String)> = postvec_core::registry::KEY_SETTINGS
        .iter()
        .map(|&(name, value)| {
            let old = Spi::get_one_with_args::<String>(
                "SELECT pg_catalog.current_setting($1)",
                &[name.into()],
            )
            .ok()
            .flatten()
            .unwrap_or_default();
            set(name, value);
            (name, old)
        })
        .collect();
    let out = f();
    for (name, old) in saved {
        set(name, &old);
    }
    out
}

pub(crate) fn with_pinned_search_path<R>(f: impl FnOnce() -> R) -> R {
    let saved = Spi::get_one::<String>("SELECT pg_catalog.current_setting('search_path')")
        .ok()
        .flatten()
        .unwrap_or_else(|| "\"$user\", public".to_string());
    Spi::run("SET LOCAL search_path TO pg_catalog")
        .unwrap_or_else(|e| error!("postvec: pinning search_path failed: {e}"));
    let out = f();
    Spi::run_with_args(
        "SELECT pg_catalog.set_config('search_path', $1, true)",
        &[saved.as_str().into()],
    )
    .unwrap_or_else(|e| error!("postvec: restoring search_path failed: {e}"));
    out
}

/// Primary-key columns in index order: `(name, formatted type, typcategory)`
/// per column. Refuses tables without a PK (`ctid` is not stable). Composite
/// PKs are supported — rows are keyed by `ROW(...)::text`.
///
/// Types are rendered under a pinned `pg_catalog` search path: the stored
/// `pk_types` become cast targets inside worker SQL, so a domain or
/// user-defined PK type visible only under the enabling caller's search path
/// must be captured schema-qualified, or every later refresh/claim would
/// fail to resolve it.
fn primary_key(rel: &RelInfo) -> Vec<(String, String, String)> {
    let cols: Vec<(String, String, String)> = with_pinned_search_path(|| {
        Spi::connect(|c| {
            let t = c
                .select(
                    // Only the first indnkeyatts entries of indkey are key
                    // columns; a PRIMARY KEY ... INCLUDE (...) payload column is
                    // not part of the row identity and must not be keyed on.
                    // int2vector subscripts are 0-based (unlike ordinary arrays),
                    // so the bound is written via array_lower rather than
                    // assuming either convention.
                    "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod),
                        t.typcategory::text
                   FROM pg_index i
                   JOIN pg_attribute a
                     ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
                   JOIN pg_type t ON t.oid = a.atttypid
                  WHERE i.indrelid = $1 AND i.indisprimary
                    AND array_position(i.indkey, a.attnum)
                        < array_lower(i.indkey, 1) + i.indnkeyatts
                  ORDER BY array_position(i.indkey, a.attnum)",
                    None,
                    &[rel.oid.into()],
                )
                .unwrap();
            t.into_iter()
                .map(|r| {
                    (
                        r.get::<String>(1).unwrap().unwrap(),
                        r.get::<String>(2).unwrap().unwrap(),
                        r.get::<String>(3).unwrap().unwrap(),
                    )
                })
                .collect()
        })
    });
    if cols.is_empty() {
        error!(
            "postvec: {}.{} has no primary key; enable() requires one \
             (ctid is not stable across UPDATE/VACUUM FULL)",
            rel.schema, rel.table
        );
    }
    cols
}

/// Format-type of a live column, or `None` if it doesn't exist.
pub(crate) fn column_type(oid: pg_sys::Oid, column: &str) -> Option<String> {
    Spi::get_one_with_args::<String>(
        "SELECT pg_catalog.format_type(atttypid, atttypmod)
           FROM pg_attribute
          WHERE attrelid = $1 AND attname = $2 AND attnum > 0 AND NOT attisdropped",
        &[oid.into(), column.into()],
    )
    .ok()
    .flatten()
}

/// Resolve the shadow-column dimension: the embed model's `target_dim`; for a
/// convert-only model (reachable via an embed-bridge route), the converter's
/// `target_dim`; else a probe embed; else refuse. The model must have *some*
/// embed path (direct or bridged) — the worker has to embed fresh writes into
/// this column forever after.
pub(crate) fn resolve_dim(model: &str) -> i32 {
    match crate::api::embed::lookup_route(model, None) {
        Ok(Some(crate::api::embed::Route { dim: Some(d), .. })) if d > 0 => return d,
        Ok(Some(_)) => {} // embed route known, dimension missing: probe below
        _ => {
            // Not an embed model. Insist on an embed route (bridge) before
            // trusting the converter's target_dim — otherwise the error is
            // the resolver's, which names what's missing.
            if let Err(e) = crate::api::embed::resolve_embed_route(model) {
                error!("postvec: {e}");
            }
            let dim = Spi::get_one_with_args::<i32>(
                "SELECT target_dim FROM postvec.models
                  WHERE model_type = 'convert' AND target_model = $1
                    AND target_dim IS NOT NULL AND target_dim > 0
                  ORDER BY name LIMIT 1",
                &[model.into()],
            )
            .ok()
            .flatten();
            if let Some(d) = dim {
                return d;
            }
        }
    }
    // Fallback: probe-embed and measure. Rides the bridge for
    // convert-only models.
    match embed_texts(
        &["dimension probe".to_string()],
        model,
        None,
        crate::client::EmbedPurpose::Document,
    ) {
        Ok(vecs) if vecs.first().map(|v| v.len()).unwrap_or(0) > 0 => vecs[0].len() as i32,
        Ok(_) => error!("postvec: probe embed for {model:?} returned an empty vector"),
        Err(e) => error!(
            "postvec: model {model:?} has no target_dim in the cache and the probe embed failed: {e}"
        ),
    }
}

/// The catalog facts `adopt()` needs about an existing vector column.
pub(crate) struct VectorColumnInfo {
    /// Declared dimension from `pg_attribute.atttypmod` (pgvector stores it
    /// there directly).
    pub(crate) dim: i32,
    pub(crate) not_null: bool,
    pub(crate) generated: bool,
}

/// The declared dimension of an existing pgvector column, or a refusal that
/// names the fix. The column's base type OID is compared with the `vector`
/// type owned by the pgvector extension (join `pg_type.typnamespace` to
/// `pg_extension.extnamespace` for `extname = 'vector'`) — not a search-path
/// lookup or `typname` alone: another schema may contain a same-named type.
/// Domains over vector, `halfvec`, arrays, and everything else are refused
/// with an `ALTER TABLE ... TYPE vector(N) USING ...` recipe; they are
/// deliberately outside the write contract.
pub(crate) fn vector_column_info(rel: &RelInfo, col: &str) -> VectorColumnInfo {
    let found: Option<(pg_sys::Oid, String, i32, bool, String, pg_sys::Oid)> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT a.atttypid,
                        pg_catalog.format_type(a.atttypid, a.atttypmod),
                        a.atttypmod,
                        a.attnotnull,
                        a.attgenerated::text,
                        vt.oid
                   FROM pg_attribute a
                   CROSS JOIN (SELECT t.oid
                                 FROM pg_type t
                                 JOIN pg_extension e ON e.extnamespace = t.typnamespace
                                WHERE e.extname = 'vector' AND t.typname = 'vector') vt
                  WHERE a.attrelid = $1 AND a.attname = $2
                    AND a.attnum > 0 AND NOT a.attisdropped",
                Some(1),
                &[rel.oid.into(), col.into()],
            )
            .unwrap();
        t.into_iter().next().map(|r| {
            (
                r.get::<pg_sys::Oid>(1).unwrap().unwrap(),
                r.get::<String>(2).unwrap().unwrap(),
                r.get::<i32>(3).unwrap().unwrap(),
                r.get::<bool>(4).unwrap().unwrap(),
                r.get::<String>(5).unwrap().unwrap_or_default(),
                r.get::<pg_sys::Oid>(6).unwrap().unwrap(),
            )
        })
    });
    let tbl = format!("{}.{}", quote_ident(&rel.schema), quote_ident(&rel.table));
    match found {
        None => error!(
            "postvec: column {col:?} does not exist on {tbl}; adopt() takes over an \
             existing vector column — use postvec.enable() to create one"
        ),
        Some((oid, t, _, _, _, vector_oid)) if oid != vector_oid => error!(
            "postvec: {col:?} is {t}, not pgvector's vector type. Convert it first, e.g.\n  \
             ALTER TABLE {tbl} ALTER COLUMN {qcol} TYPE vector(1536) \
             USING {qcol}::vector(1536);",
            qcol = quote_ident(col),
        ),
        Some((_, _, m, _, _, _)) if m < 1 => error!(
            "postvec: {col:?} is an unconstrained `vector`; postvec needs the dimension in \
             the type so search() and the index stay correct:\n  \
             ALTER TABLE {tbl} ALTER COLUMN {qcol} TYPE vector(1536);",
            qcol = quote_ident(col),
        ),
        Some((_, _, m, not_null, generated, _)) => VectorColumnInfo {
            dim: m,
            not_null,
            generated: !generated.is_empty(),
        },
    }
}

/// Every distinct positive dimension the model catalogue knows for `model`,
/// across all applicable roles: embed target (`target_dim`), converter source
/// (`source_dim` where `source_model = $1`), converter target (`target_dim`
/// where `target_model = $1`). Sorted ascending.
fn catalogue_dims(model: &str) -> Vec<i32> {
    Spi::connect(|c| {
        let t = c
            .select(
                "SELECT DISTINCT d FROM (
                     SELECT target_dim AS d FROM postvec.models
                      WHERE model_type = 'embed' AND (target_model = $1 OR name = $1)
                     UNION ALL
                     SELECT source_dim FROM postvec.models
                      WHERE model_type = 'convert' AND source_model = $1
                     UNION ALL
                     SELECT target_dim FROM postvec.models
                      WHERE model_type = 'convert' AND target_model = $1
                 ) dims WHERE d IS NOT NULL AND d > 0 ORDER BY d",
                None,
                &[model.into()],
            )
            .unwrap();
        t.into_iter()
            .map(|r| r.get::<i32>(1).unwrap().unwrap())
            .collect()
    })
}

/// Whether the model name exists in the catalogue in *any* applicable role
/// (embed target/name, converter source, converter target) — even when every
/// relevant dimension is NULL.
fn model_known(model: &str) -> bool {
    Spi::get_one_with_args::<bool>(
        "SELECT EXISTS (
             SELECT 1 FROM postvec.models
              WHERE (model_type = 'embed' AND (target_model = $1 OR name = $1))
                 OR (model_type = 'convert' AND (source_model = $1 OR target_model = $1))
         )",
        &[model.into()],
    )
    .unwrap()
    .unwrap_or(false)
}

/// The adopted column's declared dimension is the authority: it is a fact
/// about stored bytes. The catalogue is consulted only to *contradict* it —
/// a model whose known dimension differs means the attribution is wrong, and
/// a wrong attribution makes every later convert() garbage that no error will
/// ever surface. Never probe-embeds: the column already answered the question,
/// and a probe would fail for exactly the dead models adoption exists to serve.
fn adopt_dim(rel: &RelInfo, vec_col: &str, model: &str, col_dim: i32) -> i32 {
    if !model_known(model) {
        error!(
            "postvec: model {model:?} is not known to postvec.models in any role \
             (embed target, converter source, or converter target) — check for a typo, \
             or run postvec.refresh_models()"
        );
    }
    let known = catalogue_dims(model);
    if known.len() > 1 {
        error!(
            "postvec: the model catalogue disagrees about {model:?}: known dimensions are \
             {known:?}; fix the inference fleet/model cache before adoption"
        );
    }
    if let Some(known) = known.first() {
        if *known != col_dim {
            error!(
                "postvec: {vec_col:?} on {}.{} is vector({col_dim}) but {model:?} produces \
                 {known}-dimensional vectors — check the model name; adopt() trusts the \
                 column, so a wrong model silently corrupts every later migrate()",
                rel.schema, rel.table
            );
        }
    }
    col_dim
}

/// Friendly preflight for the one-writer-per-vector-column invariant: refuse
/// when another **active** registry entry already writes this vector column,
/// or a live migration's `_new` scratch column has this name. The database
/// constraint (`registry_active_vector_target_key`) remains authoritative for
/// concurrent callers; this exists to name the conflicting entry.
fn assert_column_unclaimed(schema: &str, table: &str, vec_col: &str) {
    let other: Option<String> = Spi::get_one_with_args::<String>(
        "SELECT source_column FROM postvec.registry
          WHERE table_schema = $1 AND table_name = $2 AND vector_column = $3
            AND state <> 'disabled' LIMIT 1",
        &[schema.into(), table.into(), vec_col.into()],
    )
    .ok()
    .flatten();
    if let Some(src) = other {
        error!(
            "postvec: {vec_col:?} on {schema}.{table} is already written by the postvec \
             entry for source column {src:?}; one active writer per vector column \
             (disable() that entry first)"
        );
    }
    let live_migration: Option<i64> = Spi::get_one_with_args::<i64>(
        "SELECT m.id FROM postvec.migrations m
           JOIN postvec.registry r ON r.id = m.registry_id
          WHERE r.table_schema = $1 AND r.table_name = $2
            AND m.new_column = $3
            AND m.state IN ('running','awaiting_finalize','failed')
          LIMIT 1",
        &[schema.into(), table.into(), vec_col.into()],
    )
    .ok()
    .flatten();
    if let Some(mid) = live_migration {
        error!(
            "postvec: {vec_col:?} on {schema}.{table} is migration {mid}'s scratch column; \
             finalize or abort that migration first (postvec.migration_status())"
        );
    }
}

/// Drop an entry's generated triggers, trigger functions, and postvec-built
/// indexes. Tolerant of the relation itself being gone (its triggers and
/// indexes died with it; only the functions need dropping then). Shared by
/// `disable()` and [`quarantine_entry`].
pub(crate) fn teardown_entry_objects(entry: &RegistryEntry, relation_exists: bool) {
    let id = entry.id;
    let mut ddl = String::new();
    if relation_exists {
        let qtable = entry.qualified_table();
        // "del" exists only for recursive entries; IF EXISTS makes the list
        // uniform across modes.
        for trg in ["ins", "upd", "pk", "del", "trunc"] {
            ddl.push_str(&format!(
                "DROP TRIGGER IF EXISTS {name} ON {qtable};\n",
                name = quote_ident(&format!("postvec_{trg}_{id}")),
            ));
        }
    }
    // Per-entry generated functions exist only for column-mode entries; the
    // recursive trigger functions are shared extension objects and are never
    // dropped here.
    ddl.push_str(&format!(
        "DROP FUNCTION IF EXISTS postvec.trg_ins_{id}();\n\
         DROP FUNCTION IF EXISTS postvec.trg_upd_{id}();\n\
         DROP FUNCTION IF EXISTS postvec.trg_pk_{id}();\n\
         DELETE FROM postvec.lexical_df WHERE registry_id = {id};\n\
         DELETE FROM postvec.lexical_stats WHERE registry_id = {id};",
    ));
    Spi::run(&ddl).unwrap_or_else(|e| error!("postvec: entry teardown failed: {e}"));
    // Generated indexes are dropped by *identity*, not by name: a same-named
    // user index (possible whenever postvec's own index was never built —
    // e.g. an adopted entry) must never be collateral damage. They live on
    // the entry's vector target — the destination for a recursive entry.
    for idx in ["fts", "vec"] {
        drop_owned_index(entry, &format!("postvec_{idx}_{id}"));
    }
}

/// Drop `schema.name` only when it verifiably is postvec's index for this
/// entry: it must be an index **on the entry's vector-target table** (the
/// source in column mode, the destination in recursive mode) and carry the
/// extension-dependency edge stamped at creation. A same-named object that
/// fails either check is left intact with a WARNING — teardown never removes
/// what postvec cannot prove it created (the ownership invariant).
fn drop_owned_index(entry: &RegistryEntry, name: &str) {
    let target_schema = entry
        .destination_schema
        .as_deref()
        .unwrap_or(&entry.table_schema);
    let qualified = format!("{}.{}", quote_ident(target_schema), quote_ident(name));
    let owned = Spi::get_one_with_args::<bool>(
        "SELECT i.indrelid = to_regclass($2)
                AND EXISTS (
                    SELECT 1 FROM pg_depend d
                     JOIN pg_extension e ON e.oid = d.refobjid
                    WHERE d.classid = 'pg_class'::regclass
                      AND d.objid = c.oid
                      AND d.refclassid = 'pg_extension'::regclass
                      AND d.deptype = 'x'
                      AND e.extname = 'postvec')
           FROM pg_class c
           JOIN pg_index i ON i.indexrelid = c.oid
          WHERE c.oid = to_regclass($1)",
        &[
            qualified.as_str().into(),
            entry.qualified_vector_table().as_str().into(),
        ],
    )
    .ok()
    .flatten();
    match owned {
        None => {} // no such index (or not an index at all): nothing to do
        Some(true) => {
            Spi::run(&format!("DROP INDEX {qualified}"))
                .unwrap_or_else(|e| error!("postvec: dropping index {qualified} failed: {e}"));
        }
        Some(false) => warning!(
            "postvec: leaving index {qualified} in place — it is not on {tbl} or lacks the \
             postvec extension dependency, so postvec cannot prove it created it",
            tbl = entry.qualified_vector_table(),
        ),
    }
}

/// Positive ownership and structural identity proof for the managed
/// destination tree, or the refusal reason. Delegates to
/// [`RegistryEntry::missing_destination_dependency`], which proves the
/// exact marker comments, fixed-column types/identity, the pgvector type
/// and exact dimension, the live source-PK type/typmod/collation match,
/// the unique key and view identity. A view already removed by normal
/// dependency handling (a prior source `DROP ... CASCADE`) does not
/// block proving the table.
///
/// Names are never ownership. A missing or changed marker leaves the
/// object intact, with a manual `DROP` as the operator's escape hatch.
fn destination_ownership_error(entry: &RegistryEntry) -> Option<String> {
    entry.missing_destination_dependency(&[])
}

/// Acquire the teardown locks in source -> destination -> registry order,
/// then re-run the ownership/structure proof under those locks. Only after
/// `Ok(())` may a caller update the registry or drop anything. The
/// ACCESS EXCLUSIVE locks pin the exact relations the proof validated, so
/// no concurrent transaction can swap in a same-named replacement between
/// validation and DROP. Taking the destination lock before the registry
/// row keeps the order consistent with the auto-index worker (destination
/// SHARE -> registry row) and avoids that deadlock.
///
/// Returns the refusal reason instead of raising so `uninstall()`'s sweep can
/// warn-and-continue while `disable()` raises.
fn lock_and_prove_destination(entry: &RegistryEntry) -> Result<(), String> {
    /// Resolve `qname`, take an ACCESS EXCLUSIVE lock **by OID** (never
    /// `LOCK TABLE`, which raises a transaction-aborting SQL error on
    /// unexpected relation kinds such as a same-named sequence — breaking
    /// `uninstall()`'s warn-and-continue promise), then revalidate under the
    /// lock that the relation still exists, that the name still maps to the
    /// locked OID (no concurrent swap), and that its kind is expected.
    /// `Ok(None)` = the name resolves to nothing.
    fn lock_by_name(
        qname: &str,
        expected_kinds: &[&str],
        what: &str,
    ) -> Result<Option<pg_sys::Oid>, String> {
        let oid =
            Spi::get_one_with_args::<pg_sys::Oid>("SELECT to_regclass($1)::oid", &[qname.into()])
                .ok()
                .flatten()
                .filter(|o| *o != pg_sys::Oid::INVALID);
        let Some(oid) = oid else {
            return Ok(None);
        };
        unsafe {
            pg_sys::LockRelationOid(oid, pg_sys::AccessExclusiveLock as pg_sys::LOCKMODE);
        }
        let kind = Spi::get_one_with_args::<String>(
            "SELECT relkind::text FROM pg_class WHERE oid = $1",
            &[oid.into()],
        )
        .ok()
        .flatten();
        let Some(kind) = kind else {
            // Dropped while we waited for the lock: for the caller this is
            // the same as "never existed".
            return Ok(None);
        };
        if !expected_kinds.contains(&kind.as_str()) {
            return Err(format!(
                "{what} {qname} is not the expected relation kind (relkind={kind})"
            ));
        }
        let remapped =
            Spi::get_one_with_args::<pg_sys::Oid>("SELECT to_regclass($1)::oid", &[qname.into()])
                .ok()
                .flatten()
                .filter(|o| *o != pg_sys::Oid::INVALID);
        if remapped != Some(oid) {
            return Err(format!(
                "{what} {qname} was replaced by another relation while waiting for its lock"
            ));
        }
        Ok(Some(oid))
    }

    // (1) The source first, when it still exists (its trigger teardown DDL
    // will need it anyway).
    lock_by_name(&entry.qualified_table(), &["r", "p"], "the source")?;
    // (2) The destination, then the view — whatever those names resolve to
    // right now is exactly what the proof below validates, and the locks
    // hold it still for the rest of the transaction.
    let qdest = entry.qualified_vector_table();
    if lock_by_name(&qdest, &["r"], "the destination")?.is_none() {
        return Err(entry
            .missing_destination_dependency(&[])
            .unwrap_or_else(|| format!("{qdest} does not exist")));
    }
    if let Some(qview) = entry.qualified_destination_view() {
        lock_by_name(&qview, &["v"], "the destination view")?;
    }
    // (3) Prove ownership and structure under the locks.
    match destination_ownership_error(entry) {
        None => Ok(()),
        Some(reason) => Err(reason),
    }
}

/// Drop the proven destination view (if still present) and table. Callers
/// must have passed [`destination_ownership_error`] first.
fn drop_destination_objects(entry: &RegistryEntry) {
    let qdest = entry.qualified_vector_table();
    if let Some(qview) = entry.qualified_destination_view() {
        Spi::run(&format!("DROP VIEW IF EXISTS {qview}"))
            .unwrap_or_else(|e| error!("postvec: dropping the destination view failed: {e}"));
    }
    Spi::run(&format!("DROP TABLE {qdest}"))
        .unwrap_or_else(|e| error!("postvec: dropping the destination table failed: {e}"));
    log!(
        "postvec: dropped managed destination {qdest} (registry entry {})",
        entry.id
    );
}

/// Take a broken entry out of service: an entry whose relation or columns
/// vanished (DROP TABLE / DROP COLUMN while enabled) can never be processed
/// again — without this, the worker would die on the same SPI error every
/// pass, forever. Tears down whatever objects remain, purges the entry's
/// jobs, fails any live migration, and marks the registry row `disabled`
/// (kept for audit; `enable()` clears it on re-enable). Must be called
/// inside a transaction.
pub(crate) fn quarantine_entry(entry: &RegistryEntry, reason: &str) {
    warning!(
        "postvec: quarantining {}.{}.{} (registry id {}): {reason}; \
         run postvec.enable() again once the table is back in shape",
        entry.table_schema,
        entry.table_name,
        entry.source_column,
        entry.id
    );
    let rel_exists = Spi::get_one_with_args::<bool>(
        "SELECT to_regclass($1) IS NOT NULL",
        &[entry.qualified_table().as_str().into()],
    )
    .unwrap_or(Some(false))
    .unwrap_or(false);
    teardown_entry_objects(entry, rel_exists);
    Spi::run_with_args(
        "DELETE FROM postvec.jobs WHERE registry_id = $1",
        &[entry.id.into()],
    )
    .unwrap();
    Spi::run_with_args(
        "UPDATE postvec.migrations
            SET state = 'failed', error = $2, finished_at = now()
          WHERE registry_id = $1 AND state IN ('running','awaiting_finalize')",
        &[entry.id.into(), reason.into()],
    )
    .unwrap();
    Spi::run_with_args(
        "UPDATE postvec.registry SET state = 'disabled' WHERE id = $1",
        &[entry.id.into()],
    )
    .unwrap();
}

/// Apply the `postvec.ddl_lock_timeout_ms` ceiling with SET LOCAL before a
/// lifecycle verb takes its first relation lock. A verb queued behind one
/// long-running query otherwise waits forever, and while queued its pending
/// ACCESS EXCLUSIVE blocks every later query on the table. A caller's
/// stricter (smaller, non-zero) `lock_timeout` is preserved;
/// `postvec.ddl_lock_timeout_ms = 0` disables the ceiling. `lock_timeout`
/// bounds each acquisition, not total statement time, and the SET LOCAL
/// lasts until the surrounding transaction ends.
pub(crate) fn apply_ddl_lock_timeout() {
    let ceiling = crate::gucs::DDL_LOCK_TIMEOUT_MS.get() as i64;
    if ceiling <= 0 {
        return;
    }
    // pg_settings.setting reports lock_timeout in its native unit (ms),
    // unadorned — unlike SHOW, which appends display units.
    let current = Spi::get_one::<i64>(
        "SELECT setting::bigint FROM pg_catalog.pg_settings WHERE name = 'lock_timeout'",
    )
    .ok()
    .flatten()
    .unwrap_or(0);
    if current > 0 && current <= ceiling {
        return;
    }
    Spi::run(&format!("SET LOCAL lock_timeout = {ceiling}")).unwrap_or_else(|e| {
        // A failed ceiling must be a refusal, not a silent absence of the
        // ceiling. Retrying the verb is cheap; discovering that a
        // lifecycle call waited unbounded behind a long query is not.
        error!(
            "postvec: could not apply the postvec.ddl_lock_timeout_ms ceiling              (SET LOCAL lock_timeout failed: {e}); retry the call"
        );
    });
}

/// Row ceiling for one-shot ('queue') backfill enqueues. The enqueue inserts
/// one `postvec.jobs` row per eligible source row inside the calling verb's
/// transaction, while that verb's relation locks stay held — on a large table
/// that is an unbounded lock window plus a queue-bloat spike. Above the
/// ceiling the verb refuses and points at the worker-paced alternative.
const QUEUE_BACKFILL_MAX_ROWS: i64 = 1_000_000;

/// TRUE when the table holds more than [`QUEUE_BACKFILL_MAX_ROWS`] rows.
/// Deliberately an exact-but-bounded existence probe, not a `pg_class`
/// estimate. `reltuples` is 0 on a bulk-loaded never-analyzed table and
/// arbitrarily stale after churn, so a guard rail built on it can wave
/// through exactly the table it exists for. `LIMIT max+1` caps the
/// probe's own cost: it stops the moment the ceiling is provably crossed.
fn queue_backfill_over_limit(qtable: &str) -> bool {
    // Fail closed: this probe is a safety refusal, so a failed probe must
    // abort the verb, never read as "zero rows" and wave the enqueue through.
    let probed = Spi::get_one::<i64>(&format!(
        "SELECT count(*) FROM (SELECT 1 FROM {qtable} LIMIT {lim}) s",
        lim = QUEUE_BACKFILL_MAX_ROWS + 1,
    ))
    .unwrap_or_else(|e| error!("postvec: backfill scale probe on {qtable} failed: {e}"))
    .unwrap_or_else(|| error!("postvec: backfill scale probe on {qtable} returned no row"));
    probed > QUEUE_BACKFILL_MAX_ROWS
}

fn assert_queue_backfill_scale(qtable: &str) {
    if queue_backfill_over_limit(qtable) {
        error!(
            "postvec: {qtable} holds more than {QUEUE_BACKFILL_MAX_ROWS} rows; a one-shot \
             queue backfill would enqueue them all in this transaction while the verb's \
             relation locks are held. Use backfill_mode => 'cursor' (the worker feeds \
             bounded chunks), or disable backfill here and drive it separately"
        );
    }
}

pub(crate) fn assert_owner(rel: &RelInfo) {
    if !rel.owner_ok {
        error!(
            "postvec: must own {}.{} (or be a superuser) to manage it",
            rel.schema, rel.table
        );
    }
}

/// `distance` (cosine|l2|ip) and `trigger_mode` (statement|row) validation.
pub(crate) fn validate_choice(kind: &str, value: &str, allowed: &[&str]) -> String {
    if allowed.contains(&value) {
        value.to_string()
    } else {
        error!("postvec: invalid {kind} {value:?}; expected one of {allowed:?}");
    }
}

/// Where the vector column comes from. The only axis on which `enable()`
/// and `adopt()` differ before the DDL.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin {
    /// `enable()`: postvec creates the column; the model decides the dimension.
    Created,
    /// `adopt()`: the column already exists; its declared type/dimension are
    /// read from the catalog and reconciled against the model catalogue.
    Adopted,
}

/// Everything `enable()` and `adopt()` decide identically, computed by
/// [`plan_entry`]. The planner performs no DDL and writes nothing.
pub(crate) struct EntryPlan {
    pub(crate) rel: RelInfo,
    pub(crate) pk_cols: Vec<String>,
    pub(crate) pk_types: Vec<String>,
    pub(crate) vec_col: String,
    pub(crate) dim: i32,
    /// Adopted only: the existing column's NOT NULL flag (always false for
    /// Created — postvec adds a plain nullable column).
    pub(crate) vec_not_null: bool,
}

/// The shared `enable()`/`adopt()` validation ladder: relation kind and
/// persistence, the source column, the prior (possibly stale) entry, the
/// primary key and its text-keying hazards, the vector column name, the
/// dimension, and the FTS configuration. Emits every warning. Runs no DDL and
/// writes nothing except a possible [`quarantine_entry`] of a stale prior row.
///
/// `statement_triggers` drives the partitioned-parent warning only — observed
/// (`sync => false`) adoption passes false because it attaches no DML trigger.
/// `recursive` marks a chunked-destination plan: the vector column will
/// live in the managed destination, so the Created branch skips the
/// source-side shadow-column collision refusal. Always false for adoption.
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_entry(
    rel: RelInfo,
    column_name: &str,
    vector_column: Option<String>,
    model: &str,
    fts_config: &str,
    statement_triggers: bool,
    origin: Origin,
    recursive: bool,
) -> EntryPlan {
    if rel.relkind == "p" && statement_triggers {
        // Statement-level triggers live only on the parent: DML addressed
        // directly at a partition bypasses them. Row triggers are cloned to
        // every (current and future) partition.
        warning!(
            "postvec: {}.{} is partitioned — statement-level triggers fire only for DML \
             through the parent; use trigger_mode => 'row' if clients write to partitions \
             directly",
            rel.schema,
            rel.table
        );
    }
    if rel.relkind != "r" && rel.relkind != "p" {
        error!(
            "postvec: {}.{} is not an ordinary or partitioned table (relkind={})",
            rel.schema, rel.table, rel.relkind
        );
    }
    // A temporary table only exists in the creating session; the background
    // worker runs in its own session and can never read or write it — the
    // entry would just get quarantined (or strand registry/queue rows when
    // the session exits). Refuse up front with a clear error.
    if rel.persistence == "t" {
        error!(
            "postvec: {}.{} is a temporary table; the postvec background worker runs in a \
             separate session and cannot process it",
            rel.schema, rel.table
        );
    }
    // Unlogged is allowed knowingly: crash recovery truncates the table (and
    // its shadow vectors) while postvec's logged control tables survive, so
    // queued jobs may reference vanished rows (they resolve as NULL-source).
    if rel.persistence == "u" {
        warning!(
            "postvec: {}.{} is unlogged — crash recovery truncates it while postvec's \
             control tables survive; pending jobs for vanished rows resolve to NULL vectors",
            rel.schema,
            rel.table
        );
    }

    if column_type(rel.oid, column_name).is_none() {
        error!(
            "postvec: column {column_name:?} does not exist on {}.{}",
            rel.schema, rel.table
        );
    }
    // The source column's type must be text-like or a bounded scalar: the
    // worker measures rendered lengths with octet_length(col::text) BEFORE
    // rendering, and for unbounded non-text types (jsonb, bytea, arrays,
    // composites) the cast itself would materialize the oversized value,
    // and the byte ceiling would not be a real bound. Project such values
    // through a text generated column upstream instead.
    let src_type = column_type(rel.oid, column_name).unwrap_or_default();
    if !boundable_column(rel.oid, column_name) {
        error!(
            "postvec: {}.{}.{column_name} has type {src_type:?}; postvec accepts text-like \
             and bounded scalar source columns only (its byte ceilings must be measurable \
             without rendering) — add a text generated column that projects this value and \
             enable that instead",
            rel.schema, rel.table,
        );
    }

    if let Some(prior) = RegistryEntry::load_active(&rel.schema, &rel.table, column_name) {
        // A same-name table that was dropped and recreated leaves a stale
        // entry whose triggers/columns died with the old table. Quarantine it
        // and proceed; a genuinely live entry still refuses.
        let stale = prior.missing_dependency(&[]).or_else(|| {
            prior
                .triggers_missing()
                .then(|| "its triggers are gone (table recreated?)".to_string())
        });
        match stale {
            Some(reason) => quarantine_entry(&prior, &reason),
            None => error!(
                "postvec: {}.{}.{column_name} is already enabled (disable() it first)",
                rel.schema, rel.table
            ),
        }
    }

    let pk = primary_key(&rel);
    let pk_cols: Vec<String> = pk.iter().map(|(c, _, _)| c.clone()).collect();
    let pk_types: Vec<String> = pk.iter().map(|(_, t, _)| t.clone()).collect();
    // Jobs are keyed by pk::text. Date/time renderings (typcategory D =
    // datetime, T = timespan) depend on session settings (DateStyle/
    // TimeZone); the queue tolerates that for plain PKs (native-typed joins),
    // but composite record keys compare as text — flag the hazard once, at
    // enable time.
    let datetime_pks: Vec<String> = pk
        .iter()
        .filter(|(_, _, cat)| cat == "D" || cat == "T")
        .map(|(col, typ, _)| format!("{col} ({typ})"))
        .collect();
    if !datetime_pks.is_empty() {
        warning!(
            "postvec: primary-key column(s) {} have date/time types whose text rendering \
             depends on session settings (DateStyle/TimeZone); keep those settings \
             consistent across writers{}",
            datetime_pks.join(", "),
            if pk.len() > 1 {
                " — composite keys compare by their record text"
            } else {
                ""
            }
        );
    }
    // Same hazard class for lossy floats: a writer with extra_float_digits < 1
    // renders a shortened float text that no longer round-trips to the same
    // value, silently orphaning the queued key.
    let float_pks: Vec<String> = pk
        .iter()
        .filter(|(_, typ, _)| typ == "real" || typ == "double precision")
        .map(|(col, typ, _)| format!("{col} ({typ})"))
        .collect();
    if !float_pks.is_empty() {
        warning!(
            "postvec: primary-key column(s) {} are floating-point; their text rendering \
             depends on extra_float_digits — keep it at its default (>= 1) for every role \
             that writes this table",
            float_pks.join(", ")
        );
    }
    let vec_col = vector_column.unwrap_or_else(|| format!("{column_name}_semantic"));
    if vec_col.len() > 59 {
        // NAMEDATALEN is 64: leave room for migrate()'s "<vec_col>_new" so a
        // later migration is never blocked by silent identifier truncation.
        // Applies to both origins; an adopted column is fixed by renaming it.
        error!(
            "postvec: vector column name {vec_col:?} exceeds 59 bytes; pass a shorter \
             vector_column"
        );
    }

    // Validate the text-search config eagerly (clean error before any DDL).
    if Spi::get_one_with_args::<String>("SELECT $1::regconfig::text", &[fts_config.into()])
        .ok()
        .flatten()
        .is_none()
    {
        error!("postvec: {fts_config:?} is not a valid text search configuration");
    }

    // The one origin-specific decision: Created refuses an existing column and
    // sizes it from the model; Adopted requires the column and reads its
    // declared dimension, using the catalogue only to contradict it.
    // A recursive entry's vector column lives in the (about to be created)
    // destination, so a same-named source column is irrelevant. Reserved-
    // name and fresh-table checks cover the destination side.
    let (dim, vec_not_null) = match origin {
        Origin::Created => {
            if !recursive && column_type(rel.oid, &vec_col).is_some() {
                error!(
                    "postvec: shadow column {vec_col:?} already exists on {}.{}; pass a \
                     different vector_column (or postvec.adopt() to take it over)",
                    rel.schema, rel.table
                );
            }
            (resolve_dim(model), false)
        }
        Origin::Adopted => {
            let info = vector_column_info(&rel, &vec_col);
            if info.generated {
                error!(
                    "postvec: {vec_col:?} on {}.{} is a generated column; the worker must \
                     write it — adopt a plain vector column instead",
                    rel.schema, rel.table
                );
            }
            if vec_col == column_name {
                error!(
                    "postvec: the vector column cannot be the source column {column_name:?} \
                     itself"
                );
            }
            if pk_cols.iter().any(|c| c == &vec_col) {
                error!(
                    "postvec: {vec_col:?} is part of the primary key of {}.{}; the worker \
                     must be able to rewrite it — adopt a non-key vector column",
                    rel.schema, rel.table
                );
            }
            (adopt_dim(&rel, &vec_col, model, info.dim), info.not_null)
        }
    };

    EntryPlan {
        rel,
        pk_cols,
        pk_types,
        vec_col,
        dim,
        vec_not_null,
    }
}

/// Everything `enable()`, `adopt()`, promotion and `set_format()` decide
/// identically about a template, without rerunning the whole entry planner.
pub(crate) struct ValidatedFormat {
    /// The exact validated input (stored and compared bytewise — there is no
    /// canonical serializer; `$body` vs `${body}` is a real change).
    pub(crate) format: Option<String>,
}

/// Built-in base-type OIDs whose `::text` rendering is either the value
/// itself (text family) or bounded-small (scalar types) — the set for which
/// `octet_length(col::text)` measures without materializing an unbounded
/// representation. This is what makes the byte-ceiling measurement a REAL
/// bound: for jsonb/bytea/arrays/composites the cast itself allocates the
/// oversized rendering, so those must be projected through a text
/// (e.g. generated) column upstream instead.
///
/// Classification is by `pg_type` OID, never by display name. Name prefixes
/// admitted every array form PostgreSQL renders with a matching prefix
/// (`numeric[]`, `character varying[]`, `timestamp ...[]`, `interval[]`)
/// and any custom type whose name happens to start with one.
/// An explicit OID allow-list rejects arrays, composites, enums, ranges and
/// custom base types structurally — they simply are not in it — and
/// [`column_base_type_oid`] unwraps domains to the base type first.
pub(crate) fn boundable_base_type_oid(oid: pg_sys::Oid) -> bool {
    [
        pg_sys::TEXTOID,
        pg_sys::NAMEOID,
        pg_sys::BOOLOID,
        pg_sys::UUIDOID,
        pg_sys::INT2OID,
        pg_sys::INT4OID,
        pg_sys::INT8OID,
        pg_sys::FLOAT4OID,
        pg_sys::FLOAT8OID,
        pg_sys::NUMERICOID,
        pg_sys::BPCHAROID,
        pg_sys::VARCHAROID,
        pg_sys::DATEOID,
        pg_sys::TIMEOID,
        pg_sys::TIMETZOID,
        pg_sys::TIMESTAMPOID,
        pg_sys::TIMESTAMPTZOID,
        pg_sys::INTERVALOID,
    ]
    .contains(&oid)
}

/// The column's base type OID with domains recursively unwrapped (bounded
/// depth — `CREATE DOMAIN` chains cannot recurse in practice, the bound is
/// belt-and-braces). `None` when the column is missing or the catalog probe
/// fails; callers treat `None` as NOT boundable (fail closed).
pub(crate) fn column_base_type_oid(attrelid: pg_sys::Oid, column: &str) -> Option<pg_sys::Oid> {
    Spi::get_one_with_args::<pg_sys::Oid>(
        "WITH RECURSIVE unwrap(oid, depth) AS (
             SELECT a.atttypid, 0
               FROM pg_catalog.pg_attribute a
              WHERE a.attrelid = $1 AND a.attname = $2
                AND a.attnum > 0 AND NOT a.attisdropped
             UNION ALL
             SELECT t.typbasetype, u.depth + 1
               FROM pg_catalog.pg_type t JOIN unwrap u ON t.oid = u.oid
              WHERE t.typtype = 'd' AND u.depth < 32
         )
         SELECT u.oid
           FROM unwrap u JOIN pg_catalog.pg_type t ON t.oid = u.oid
          WHERE t.typtype <> 'd'
          ORDER BY u.depth DESC
          LIMIT 1",
        &[attrelid.into(), column.into()],
    )
    .ok()
    .flatten()
}

/// Fail-closed byte-boundability check for one live column.
pub(crate) fn boundable_column(attrelid: pg_sys::Oid, column: &str) -> bool {
    column_base_type_oid(attrelid, column).is_some_and(boundable_base_type_oid)
}

/// Parse the template once and resolve every referenced column against the
/// live relation:
///
/// - column mode: the source column must be referenced (the CASE anchor
///   alone is not enough — a template that never embeds the source text is
///   almost certainly a mistake, and re-enqueue-on-source-change would be
///   meaningless);
/// - recursive mode: the reserved `$chunk` pseudo-column must be
///   referenced at least once (it resolves to destination `chunk_text`),
///   and the source document column must not be referenced. Embedding the
///   whole document next to each chunk defeats the context bound; `$chunk`
///   is the spelling for the document text. A real source column named
///   `chunk` cannot be referenced as context; the pseudo-variable wins
/// - neither the live vector column nor `<vector_column>_new` may be
///   referenced (a postvec-written column in the embedding input would be
///   a write/enqueue feedback loop)
/// - every other referenced column must exist, not be dropped and carry a
///   byte-boundable base type ([`boundable_base_type_oid`]). PK, generated,
///   nullable and domain columns are fine: they render through `::text`
///   (NULL context becomes an empty string).
pub(crate) fn validate_format(
    rel: &RelInfo,
    source_column: &str,
    vector_column: &str,
    format: Option<&str>,
    recursive: bool,
) -> ValidatedFormat {
    let Some(template) = format else {
        return ValidatedFormat { format: None };
    };
    let segs = crate::registry::parse_format(template)
        .unwrap_or_else(|e| error!("postvec: invalid format template: {e}"));
    let refs = crate::registry::format_referenced_columns(&segs);
    if recursive {
        if !refs.iter().any(|c| c == "chunk") {
            error!(
                "postvec: a chunked entry's format template must reference the reserved \
                 $chunk pseudo-column at least once (it is the chunk text being embedded)"
            );
        }
        if refs.iter().any(|c| c == source_column) {
            error!(
                "postvec: a chunked entry's format template must not reference the source \
                 document column {source_column:?} — that would embed the whole document \
                 next to every chunk; use $chunk for the chunk text"
            );
        }
    } else if !refs.iter().any(|c| c == source_column) {
        error!(
            "postvec: the format template must reference the source column \
             {source_column:?} (its NULL-ness anchors the vector lifecycle)"
        );
    }
    let scratch = format!("{vector_column}_new");
    for c in &refs {
        if recursive && c == "chunk" {
            continue; // the pseudo-column; never a source attribute
        }
        if c == vector_column || c == &scratch {
            error!(
                "postvec: the format template must not reference {c:?} — a postvec-written \
                 vector column in the embedding input would be a write/enqueue feedback loop"
            );
        }
        match column_type(rel.oid, c) {
            None => error!(
                "postvec: the format template references column {c:?}, which does not exist \
                 on {}.{}",
                rel.schema, rel.table
            ),
            Some(t) if !boundable_column(rel.oid, c) => error!(
                "postvec: the format template references column {c:?} of type {t:?}; \
                 templates accept text-like and bounded scalar columns only (byte ceilings \
                 must be measurable without rendering) — project the value through a text \
                 generated column instead",
            ),
            Some(_) => {}
        }
    }
    ValidatedFormat {
        format: Some(template.to_string()),
    }
}

/// Clear a previously-disabled entry's audit row (it occupies the
/// UNIQUE(schema,table,source_column) key) and insert the fresh registry row.
/// Shared by `enable()` and `adopt()`; runs in the caller's transaction.
///
/// A unique violation here is the authoritative one-writer-per-vector-column
/// invariant firing (`registry_active_vector_target_key`) — the friendly
/// preflight [`assert_column_unclaimed`] covers the sequential case, this
/// covers concurrent callers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_registry_row(
    plan: &EntryPlan,
    column_name: &str,
    model: &str,
    fts_config: &str,
    create_fts_index: bool,
    distance: &str,
    trigger_mode: &str,
    backfill_mode: &str,
    owns_vector_column: bool,
    format: Option<&str>,
    index_mode: &str,
    chunk_spec: Option<&ChunkingSpec>,
) -> i64 {
    Spi::run_with_args(
        "DELETE FROM postvec.registry
          WHERE table_schema = $1 AND table_name = $2
            AND source_column = $3 AND state = 'disabled'",
        &[
            plan.rel.schema.as_str().into(),
            plan.rel.table.as_str().into(),
            column_name.into(),
        ],
    )
    .unwrap_or_else(|e| error!("postvec: clearing prior disabled entry failed: {e}"));

    let tbl = format!("{}.{}", plan.rel.schema, plan.rel.table);
    let vec_col = plan.vec_col.clone();
    let (chunking, size, overlap, dest_schema, dest_table, dest_view, dest_token) = match chunk_spec
    {
        Some(s) => (
            "recursive",
            Some(s.size),
            Some(s.overlap),
            Some(plan.rel.schema.clone()),
            Some(s.destination.clone()),
            Some(s.view.clone()),
            Some(s.token.clone()),
        ),
        None => ("none", None, None, None, None, None, None),
    };
    pgrx::PgTryBuilder::new(|| {
        Spi::get_one_with_args::<i64>(
            "INSERT INTO postvec.registry
                 (table_schema, table_name, source_column, vector_column,
                  pk_columns, pk_types, model, space, dim, fts_config, create_fts_index,
                  distance, trigger_mode, backfill_mode, owns_vector_column, format,
                  index_mode, chunking, chunk_size, chunk_overlap,
                  destination_schema, destination_table, destination_view,
                  destination_token)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10::regconfig,$11,$12,$13,$14,$15,$16,
                     $17,$18,$19,$20,$21,$22,$23,$24)
             RETURNING id",
            &[
                plan.rel.schema.as_str().into(),
                plan.rel.table.as_str().into(),
                column_name.into(),
                plan.vec_col.as_str().into(),
                plan.pk_cols.clone().into(),
                plan.pk_types.clone().into(),
                model.into(),
                crate::api::embed::cache_space(model).into(),
                plan.dim.into(),
                fts_config.into(),
                create_fts_index.into(),
                distance.into(),
                trigger_mode.into(),
                backfill_mode.into(),
                owns_vector_column.into(),
                format.into(),
                index_mode.into(),
                chunking.into(),
                size.into(),
                overlap.into(),
                dest_schema.as_deref().into(),
                dest_table.as_deref().into(),
                dest_view.as_deref().into(),
                dest_token.as_deref().into(),
            ],
        )
        .unwrap()
        .expect("registry insert returned no id")
    })
    .catch_when(pgrx::PgSqlErrorCode::ERRCODE_UNIQUE_VIOLATION, move |_| {
        error!(
            "postvec: another active postvec entry already writes {vec_col:?} on {tbl} \
                 (a concurrent enable()/adopt() raced this call); see postvec.registry"
        )
    })
    .execute()
}

/// The resolved recursive-chunking request, validated before any DDL or
/// registry write.
pub(crate) struct ChunkingSpec {
    pub(crate) size: i32,
    pub(crate) overlap: i32,
    /// Bare (unqualified) destination table name; lives in the source schema.
    pub(crate) destination: String,
    /// `<destination>_view`.
    pub(crate) view: String,
    /// The unpredictable ownership token written into the destination/view
    /// comments.
    pub(crate) token: String,
}

/// Reserved destination column names the operator's `vector_column` must not
/// collide with.
const RESERVED_DESTINATION_COLUMNS: [&str; 6] = [
    "postvec_chunk_id",
    "postvec_source_pk",
    "postvec_chunk_seq",
    "postvec_char_start",
    "postvec_char_end",
    "chunk_text",
];

/// Validate the four recursive `enable()` arguments into `None` (column mode) or a
/// [`ChunkingSpec`]. Pure argument validation — schema-dependent checks
/// (single-column PK, schema CREATE privilege, name collisions) run after
/// `plan_entry` in [`validate_recursive_plan`].
fn resolve_chunking_args(
    chunking: &str,
    chunk_size: Option<i32>,
    chunk_overlap: Option<i32>,
    destination: Option<&str>,
    index_mode: &str,
) -> Option<ChunkingSpec> {
    let chunking = validate_choice("chunking", chunking, &["none", "recursive"]);
    if chunking == "none" {
        if chunk_size.is_some() || chunk_overlap.is_some() || destination.is_some() {
            error!(
                "postvec: chunk_size, chunk_overlap, and destination require \
                 chunking => 'recursive'"
            );
        }
        return None;
    }
    let Some(destination) = destination else {
        error!(
            "postvec: chunking => 'recursive' requires destination => '<table name>' — the \
             managed 1:N chunk table postvec will create in the source schema"
        );
    };
    if destination.is_empty() || destination.contains('.') {
        error!(
            "postvec: destination {destination:?} must be one unqualified table name; the \
             destination is always created in the source table's schema"
        );
    }
    // NAMEDATALEN is 64 (63 usable bytes): leave room for the "_view" suffix
    // so neither generated identifier is silently truncated.
    if destination.len() > 58 {
        error!(
            "postvec: destination {destination:?} exceeds 58 bytes; the generated \
             \"{destination}_view\" name would be truncated — pass a shorter destination"
        );
    }
    let size = chunk_size.unwrap_or(crate::chunking::DEFAULT_CHUNK_SIZE);
    let overlap = chunk_overlap.unwrap_or(crate::chunking::DEFAULT_CHUNK_OVERLAP);
    if !(crate::chunking::MIN_CHUNK_SIZE..=crate::chunking::MAX_CHUNK_SIZE).contains(&size) {
        error!(
            "postvec: chunk_size must be between {} and {} characters (got {size})",
            crate::chunking::MIN_CHUNK_SIZE,
            crate::chunking::MAX_CHUNK_SIZE,
        );
    }
    if overlap < 0 || overlap >= size {
        error!(
            "postvec: chunk_overlap must be between 0 and chunk_size - 1 (got {overlap} for \
             chunk_size {size}; the defaults are {}/{})",
            crate::chunking::DEFAULT_CHUNK_SIZE,
            crate::chunking::DEFAULT_CHUNK_OVERLAP,
        );
    }
    // Beyond 75% overlap, the asymptotic amplification of separator-poor
    // text exceeds the splitter's output budget, so sufficiently long
    // documents dead-letter. Short documents and boundary-rich text (where
    // incoming pieces shrink the carried overlap) can still fit, so this is
    // an advisory, not a refusal — but it is almost certainly a
    // misconfiguration.
    if overlap as i64 * 4 > size as i64 * 3 {
        warning!(
            "postvec: chunk_overlap {overlap} exceeds 75% of chunk_size {size}; \
             sufficiently long documents may exceed the splitter's {}x output budget \
             and dead-letter — lower the overlap or raise chunk_size",
            crate::chunking::MAX_OUTPUT_AMPLIFICATION,
        );
    }
    if index_mode == "immediate" {
        // Deliberate asymmetry with column mode: 'immediate' is documented as
        // intent/audit state with no later reconciliation, and on the
        // provably empty destination it would build an index that never sees
        // the data it was asked for.
        error!(
            "postvec: index_mode => 'immediate' is not available with chunking => \
             'recursive' (the destination is empty at enable() time and 'immediate' never \
             reconciles later); use index_mode => 'auto', or keep 'manual' and CREATE INDEX \
             CONCURRENTLY after the backfill drains"
        );
    }
    let token = Spi::get_one::<String>(
        "SELECT md5(random()::text || clock_timestamp()::text || pg_backend_pid()::text)",
    )
    .ok()
    .flatten()
    .unwrap_or_else(|| error!("postvec: generating the destination ownership token failed"));
    Some(ChunkingSpec {
        size,
        overlap,
        destination: destination.to_string(),
        view: format!("{destination}_view"),
        token,
    })
}

/// The schema-dependent half of recursive validation, after [`plan_entry`]:
/// single-column PK, schema CREATE privilege, fresh destination/view names,
/// and no vector-column collision with the fixed destination columns.
fn validate_recursive_plan(plan: &EntryPlan, spec: &ChunkingSpec) {
    if plan.pk_cols.len() != 1 {
        error!(
            "postvec: chunking => 'recursive' requires a single-column primary key; {}.{} \
             has a composite key {:?} (composite-PK chunking is a deliberate v1 refusal)",
            plan.rel.schema, plan.rel.table, plan.pk_cols
        );
    }
    // Table ownership does not imply CREATE on the schema; the destination
    // DDL below would fail half-way without this check.
    let can_create = Spi::get_one_with_args::<bool>(
        "SELECT pg_catalog.has_schema_privilege(current_user, $1, 'CREATE')",
        &[plan.rel.schema.as_str().into()],
    )
    .unwrap()
    .unwrap_or(false);
    if !can_create {
        error!(
            "postvec: creating the destination requires CREATE on schema {:?} (table \
             ownership alone does not imply it)",
            plan.rel.schema
        );
    }
    for name in [&spec.destination, &spec.view] {
        let qname = format!("{}.{}", quote_ident(&plan.rel.schema), quote_ident(name));
        let taken = Spi::get_one_with_args::<bool>(
            "SELECT to_regclass($1) IS NOT NULL",
            &[qname.as_str().into()],
        )
        .unwrap()
        .unwrap_or(false);
        if taken {
            error!(
                "postvec: {qname} already exists; postvec never adopts or overwrites a \
                 destination — pass a different destination (or drop the existing relation \
                 if it is a leftover you own)"
            );
        }
    }
    if RESERVED_DESTINATION_COLUMNS.contains(&plan.vec_col.as_str()) {
        error!(
            "postvec: vector_column {vec:?} collides with a reserved destination column; \
             pass a different vector_column",
            vec = plan.vec_col,
        );
    }
}

/// The source PK attribute's exact type (typmod included) and, when it is not
/// the type's default, its schema-qualified collation — both rendered under a
/// pinned `pg_catalog` search path so the spelling is catalog-qualified and
/// session-independent ([R3-5]). The destination's source-key column is
/// created from this, never from the registry's stored `pk_types` text.
fn source_pk_column_definition(rel_oid: pg_sys::Oid, pk_col: &str) -> String {
    let found: Option<(String, Option<String>)> = with_pinned_search_path(|| {
        Spi::connect(|c| {
            let t = c
                .select(
                    "SELECT pg_catalog.format_type(a.atttypid, a.atttypmod),
                        CASE WHEN a.attcollation <> 0
                                  AND a.attcollation IS DISTINCT FROM t.typcollation
                             THEN pg_catalog.quote_ident(cn.nspname) || '.' ||
                                  pg_catalog.quote_ident(co.collname)
                        END
                   FROM pg_attribute a
                   JOIN pg_type t ON t.oid = a.atttypid
                   LEFT JOIN pg_collation co ON co.oid = a.attcollation
                   LEFT JOIN pg_namespace cn ON cn.oid = co.collnamespace
                  WHERE a.attrelid = $1 AND a.attname = $2
                    AND a.attnum > 0 AND NOT a.attisdropped",
                    Some(1),
                    &[rel_oid.into(), pk_col.into()],
                )
                .unwrap();
            t.into_iter().next().map(|r| {
                (
                    r.get::<String>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap(),
                )
            })
        })
    });
    let Some((typ, collation)) = found else {
        error!("postvec: primary-key column {pk_col:?} vanished during enable()");
    };
    match collation {
        Some(coll) => format!("{typ} COLLATE {coll}"),
        None => typ,
    }
}

/// Create the managed destination table, its FORCE-RLS source-visibility
/// policy, the lean security-invoker/barrier join view, the ownership-token
/// comments, and transfer both objects to the source table's owner.
/// Runs after the registry insert (the comments name the entry id).
fn create_destination_objects(entry: &RegistryEntry, rel: &RelInfo) {
    let qdest = entry.qualified_vector_table();
    let qview = entry
        .qualified_destination_view()
        .expect("recursive entry has a view");
    let qsrc = entry.qualified_table();
    let pk = quote_ident(&entry.pk_columns[0]);
    let pk_def = source_pk_column_definition(rel.oid, &entry.pk_columns[0]);
    // Match source persistence: an unlogged source gets an unlogged
    // destination, so a crash cannot leave durable chunks for source rows
    // PostgreSQL discarded.
    let unlogged = if rel.persistence == "u" {
        "UNLOGGED "
    } else {
        ""
    };
    // Deliberately no foreign key: ON DELETE CASCADE would silently
    // take over cleanup from the triggers the lifecycle depends on, and a
    // source TRUNCATE would start failing with "cannot truncate a table
    // referenced in a foreign key constraint". Enabling a chunked entry
    // must not change what a user's TRUNCATE does.
    Spi::run(&format!(
        "CREATE {unlogged}TABLE {qdest} (
             postvec_chunk_id   bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             postvec_source_pk  {pk_def} NOT NULL,
             postvec_chunk_seq  integer NOT NULL CHECK (postvec_chunk_seq >= 0),
             postvec_char_start bigint  NOT NULL CHECK (postvec_char_start >= 0),
             postvec_char_end   bigint  NOT NULL
                                CHECK (postvec_char_end > postvec_char_start),
             chunk_text         text    NOT NULL,
             {vec}              vector({dim}),
             UNIQUE (postvec_source_pk, postvec_chunk_seq)
         )",
        vec = quote_ident(&entry.vector_column),
        dim = entry.dim,
    ))
    .unwrap_or_else(|e| error!("postvec: creating the destination table failed: {e}"));

    // FORCE keeps direct reads on the derived table inside the policy even
    // for its owner; the bootstrap-superuser worker still bypasses it. The
    // policy's USING clause is the same indexed source-PK EXISTS the view
    // join uses, so caller RLS/visibility on the source decides chunk
    // visibility.
    Spi::run(&format!(
        "ALTER TABLE {qdest} ENABLE ROW LEVEL SECURITY;
         ALTER TABLE {qdest} FORCE ROW LEVEL SECURITY;
         CREATE POLICY postvec_source_visible ON {qdest} FOR SELECT
             USING (EXISTS (SELECT 1 FROM {qsrc} s
                             WHERE s.{pk} = postvec_source_pk))",
    ))
    .unwrap_or_else(|e| error!("postvec: destination row security setup failed: {e}"));

    // The lean view: chunk fields only. No vector column (it would be a
    // dependent of exactly the column migration_finalize() drops) and no
    // d.*/to_jsonb(d) (a whole-row dependency can block unrelated source
    // DDL). The source join exists solely so source existence and caller
    // RLS decide which chunks are visible.
    Spi::run(&format!(
        "CREATE VIEW {qview}
              WITH (security_invoker = true, security_barrier = true) AS
         SELECT c.postvec_source_pk  AS pk_value,
                c.postvec_chunk_seq  AS chunk_seq,
                c.postvec_char_start AS chunk_start,
                c.postvec_char_end   AS chunk_end,
                c.chunk_text
           FROM {qdest} c
           JOIN {qsrc} d ON d.{pk} = c.postvec_source_pk",
    ))
    .unwrap_or_else(|e| error!("postvec: creating the destination view failed: {e}"));

    // Ownership is proven by these exact comments, never by name. An
    // OID would be an exact identity check needing no token, but OIDs do not
    // survive pg_dump/restore — comments do. A user who edits or clears the
    // comment permanently disarms *automatic* teardown for the object, which
    // fails as a refusal with the manual DROP printed.
    let (table_comment, view_comment) = entry
        .destination_comments()
        .expect("recursive entry has a token");
    Spi::run(&format!(
        "COMMENT ON TABLE {qdest} IS {tc};
         COMMENT ON VIEW {qview} IS {vc}",
        tc = crate::registry::quote_literal_estring(&table_comment),
        vc = crate::registry::quote_literal_estring(&view_comment),
    ))
    .unwrap_or_else(|e| error!("postvec: commenting the destination failed: {e}"));

    // Both objects belong to the source table's owner — including when a
    // superuser ran enable() on that owner's behalf. No automatic grants:
    // owner-only is the safe default, and the documented application grant is
    // an explicit GRANT SELECT on both objects.
    let owner = Spi::get_one_with_args::<String>(
        "SELECT r.rolname::text FROM pg_class c
           JOIN pg_roles r ON r.oid = c.relowner
          WHERE c.oid = $1",
        &[rel.oid.into()],
    )
    .ok()
    .flatten()
    .unwrap_or_else(|| error!("postvec: resolving the source table owner failed"));
    Spi::run(&format!(
        "ALTER TABLE {qdest} OWNER TO {o};
         ALTER VIEW {qview} OWNER TO {o}",
        o = quote_ident(&owner),
    ))
    .unwrap_or_else(|e| error!("postvec: transferring destination ownership failed: {e}"));
}

/// Attach the recursive source triggers. Per-entry DDL is only
/// `CREATE TRIGGER`. The bodies are the shared extension-installed
/// SECURITY DEFINER functions, parameterized by registry id (and, for the
/// statement-mode UPDATE change set, the referenced column names, which
/// the function renders through format('%I') so they can only select
/// columns).
fn create_chunk_triggers(entry: &RegistryEntry, mode: &str) {
    let id = entry.id;
    let qtable = entry.qualified_table();
    let refs = entry
        .referenced_columns()
        .unwrap_or_else(|e| error!("postvec: stored format template is invalid: {e}"));
    let id_arg = crate::registry::quote_literal_estring(&id.to_string());
    let pk = quote_ident(&entry.pk_columns[0]);
    let ins_trg = quote_ident(&format!("postvec_ins_{id}"));
    let upd_trg = quote_ident(&format!("postvec_upd_{id}"));
    let pk_trg = quote_ident(&format!("postvec_pk_{id}"));
    let del_trg = quote_ident(&format!("postvec_del_{id}"));

    let ddl = if mode == "row" {
        let row_of_list = refs
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        let row_when_pred = refs
            .iter()
            .map(|c| {
                let q = quote_ident(c);
                format!("NEW.{q} IS DISTINCT FROM OLD.{q}")
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        format!(
            "CREATE TRIGGER {ins_trg} AFTER INSERT ON {qtable}
                 FOR EACH ROW EXECUTE FUNCTION postvec.trg_chunk_ins({id_arg});
             CREATE TRIGGER {upd_trg} AFTER UPDATE OF {row_of_list} ON {qtable}
                 FOR EACH ROW WHEN ({row_when_pred})
                 EXECUTE FUNCTION postvec.trg_chunk_upd({id_arg});
             CREATE TRIGGER {pk_trg} AFTER UPDATE OF {pk} ON {qtable}
                 FOR EACH ROW WHEN (OLD.{pk} IS DISTINCT FROM NEW.{pk})
                 EXECUTE FUNCTION postvec.trg_chunk_pk({id_arg});
             CREATE TRIGGER {del_trg} AFTER DELETE ON {qtable}
                 FOR EACH ROW EXECUTE FUNCTION postvec.trg_chunk_del({id_arg});"
        )
    } else {
        // Statement mode: transition tables forbid an UPDATE column list, so
        // the change set travels as trigger arguments into the shared body.
        let upd_args = std::iter::once(id_arg.clone())
            .chain(
                refs.iter()
                    .map(|c| crate::registry::quote_literal_estring(c)),
            )
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE TRIGGER {ins_trg} AFTER INSERT ON {qtable}
                 REFERENCING NEW TABLE AS new_table
                 FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_chunk_ins({id_arg});
             CREATE TRIGGER {upd_trg} AFTER UPDATE ON {qtable}
                 REFERENCING OLD TABLE AS old_table NEW TABLE AS new_table
                 FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_chunk_upd({upd_args});
             CREATE TRIGGER {pk_trg} AFTER UPDATE OF {pk} ON {qtable}
                 FOR EACH ROW WHEN (OLD.{pk} IS DISTINCT FROM NEW.{pk})
                 EXECUTE FUNCTION postvec.trg_chunk_pk({id_arg});
             CREATE TRIGGER {del_trg} AFTER DELETE ON {qtable}
                 REFERENCING OLD TABLE AS old_table
                 FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_chunk_del({id_arg});"
        )
    };
    Spi::run(&ddl).unwrap_or_else(|e| error!("postvec: chunk trigger creation failed: {e}"));

    let dep = format!(
        "ALTER TRIGGER {ins_trg} ON {qtable} DEPENDS ON EXTENSION postvec;\n\
         ALTER TRIGGER {upd_trg} ON {qtable} DEPENDS ON EXTENSION postvec;\n\
         ALTER TRIGGER {pk_trg} ON {qtable} DEPENDS ON EXTENSION postvec;\n\
         ALTER TRIGGER {del_trg} ON {qtable} DEPENDS ON EXTENSION postvec;"
    );
    if let Err(e) = Spi::run(&dep) {
        warn_dependency_marking_failed(&e);
    }
}

/// `if_not_exists`: the entry's id when every requested option matches it;
/// otherwise refuse, naming each difference.
fn existing_entry_id(prior: &RegistryEntry, mut requested: serde_json::Value) -> i64 {
    if let Some(fts) = requested["fts_config"].as_str().map(str::to_string) {
        requested["fts_config"] =
            Spi::get_one_with_args::<String>("SELECT $1::regconfig::text", &[fts.as_str().into()])
                .unwrap_or_else(|e| error!("postvec: invalid fts_config {fts:?}: {e}"))
                .into();
    }
    let stored = serde_json::to_value(prior).unwrap_or_default();
    let drift: Vec<String> = requested
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, want)| stored.get(key.as_str()) != Some(*want))
        .map(|(key, want)| format!("{key} {} (requested {want})", stored[key.as_str()]))
        .collect();
    if drift.is_empty() {
        return prior.id;
    }
    error!(
        "postvec: {}.{}.{} is already enabled with {}; disable() it first to change them",
        prior.table_schema,
        prior.table_name,
        prior.source_column,
        drift.join(", ")
    );
}

#[pg_extern]
#[allow(clippy::too_many_arguments)]
fn enable(
    relation: &str,
    column_name: &str,
    model: &str,
    vector_column: default!(Option<String>, "NULL"),
    fts_config: default!(&str, "'pg_catalog.english'"),
    create_fts_index: default!(bool, false),
    backfill: default!(bool, true),
    distance: default!(&str, "'cosine'"),
    trigger_mode: default!(&str, "'statement'"),
    index_mode: default!(&str, "'manual'"),
    backfill_mode: default!(&str, "'queue'"),
    format: default!(Option<String>, "NULL"),
    chunking: default!(&str, "'none'"),
    chunk_size: default!(Option<i32>, "NULL"),
    chunk_overlap: default!(Option<i32>, "NULL"),
    destination: default!(Option<String>, "NULL"),
    if_not_exists: default!(bool, false),
) -> i64 {
    apply_ddl_lock_timeout();
    let rel = resolve_relation(relation);
    assert_owner(&rel);
    // Pin the relation's definition before any catalogue classification,
    // same as adopt(): ACCESS SHARE, held to commit, blocks a concurrent
    // ALTER COLUMN TYPE between the byte-boundability checks and the
    // trigger/column DDL.
    Spi::run(&format!(
        "LOCK TABLE {}.{} IN ACCESS SHARE MODE",
        quote_ident(&rel.schema),
        quote_ident(&rel.table)
    ))
    .unwrap_or_else(|e| error!("postvec: locking the relation for enable failed: {e}"));

    let distance = validate_choice("distance", distance, &["cosine", "l2", "ip"]);
    let trigger_mode = validate_choice("trigger_mode", trigger_mode, &["statement", "row"]);
    let index_mode = validate_choice("index_mode", index_mode, &["manual", "immediate", "auto"]);
    let backfill_mode = if backfill {
        validate_choice("backfill_mode", backfill_mode, &["queue", "cursor"])
    } else {
        "none".to_string()
    };
    let chunk_spec = resolve_chunking_args(
        chunking,
        chunk_size,
        chunk_overlap,
        destination.as_deref(),
        &index_mode,
    );
    let recursive = chunk_spec.is_some();
    if if_not_exists {
        if let Some(prior) = RegistryEntry::load_active(&rel.schema, &rel.table, column_name) {
            if prior.missing_dependency(&[]).is_none() && !prior.triggers_missing() {
                let spec = chunk_spec.as_ref();
                return existing_entry_id(
                    &prior,
                    serde_json::json!({
                        "model": model,
                        "vector_column": vector_column
                            .clone()
                            .unwrap_or_else(|| format!("{column_name}_semantic")),
                        "distance": distance,
                        "trigger_mode": trigger_mode,
                        "index_mode": index_mode,
                        "fts_config": fts_config,
                        "format": format,
                        "chunking": if recursive { "recursive" } else { "none" },
                        "chunk_size": spec.map(|s| s.size),
                        "chunk_overlap": spec.map(|s| s.overlap),
                        "destination_table": spec.map(|s| &s.destination),
                    }),
                );
            }
        }
    }

    let plan = plan_entry(
        rel,
        column_name,
        vector_column,
        model,
        fts_config,
        trigger_mode == "statement",
        Origin::Created,
        recursive,
    );
    let (vec_col, dim) = (plan.vec_col.clone(), plan.dim);
    // Informed consent before any DDL: the moment this column is declared,
    // its text is bound for the named external provider.
    notice_external_provider(model, column_name);
    let vf = validate_format(
        &plan.rel,
        column_name,
        &plan.vec_col,
        format.as_deref(),
        recursive,
    );
    if let Some(spec) = &chunk_spec {
        validate_recursive_plan(&plan, spec);
    }

    // ---- DDL (atomic within the caller's transaction) ----
    let qtable = format!(
        "{}.{}",
        quote_ident(&plan.rel.schema),
        quote_ident(&plan.rel.table)
    );
    assert_index_mode_dimension(&index_mode, dim, &qtable, &vec_col, &distance);

    if !recursive {
        Spi::run(&format!(
            "ALTER TABLE {qtable} ADD COLUMN {vec} vector({dim})",
            vec = quote_ident(&vec_col),
        ))
        .unwrap_or_else(|e| error!("postvec: ADD COLUMN failed: {e}"));
    }

    let id = insert_registry_row(
        &plan,
        column_name,
        model,
        fts_config,
        create_fts_index,
        &distance,
        &trigger_mode,
        &backfill_mode,
        true,
        vf.format.as_deref(),
        &index_mode,
        chunk_spec.as_ref(),
    );

    let entry = RegistryEntry::load(id).expect("registry entry just inserted");
    if recursive {
        create_destination_objects(&entry, &plan.rel);
        create_chunk_triggers(&entry, &trigger_mode);
        create_truncate_sentinel(&entry);
    } else {
        create_triggers(&entry, &trigger_mode);
    }

    if create_fts_index {
        if recursive {
            // Recursive lexical search runs over destination chunk_text.
            create_chunk_fts_index_for(&entry);
        } else {
            create_fts_index_for(id, &qtable, column_name, fts_config);
        }
    }

    // 'immediate' runs an ordinary CREATE INDEX synchronously in this
    // transaction, readiness-first. 'auto' defers to the worker (after the
    // entry's work drains); 'manual' never builds. Recursive entries refuse
    // 'immediate' up front.
    if index_mode == "immediate" {
        ensure_vector_index(&entry);
    }

    // 'queue': one-shot enqueue of every existing row (simple, bloats the
    // queue on huge tables). 'cursor': the worker enqueues watermark-ordered
    // chunks as the queue drains. A recursive entry enqueues one local
    // `refresh` per document instead of embed jobs.
    if backfill_mode == "queue" {
        assert_queue_backfill_scale(&entry.qualified_table());
        let op = if recursive { "refresh" } else { "embed" };
        with_key_format(|| {
            Spi::run(&format!(
                "INSERT INTO postvec.jobs (registry_id, pk_value, op)
             SELECT {id}, {pk}, '{op}' FROM {qtable} WHERE {col} IS NOT NULL
             ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING",
                pk = entry.pk_text_expr(""),
                col = quote_ident(column_name),
            ))
        })
        .unwrap_or_else(|e| error!("postvec: backfill enqueue failed: {e}"));
    }
    crate::worker::worker_kick();

    match &chunk_spec {
        Some(spec) => log!(
            "postvec: enabled {}.{}.{column_name} -> {dest}.{vec_col} vector({dim}) \
             (model={model}, id={id}, chunking=recursive {size}/{overlap}, \
             backfill={backfill_mode}). The destination and its view are owner-only by \
             default: GRANT SELECT on both to application roles that should read chunks",
            plan.rel.schema,
            plan.rel.table,
            dest = spec.destination,
            size = spec.size,
            overlap = spec.overlap,
        ),
        None => log!(
            "postvec: enabled {}.{}.{column_name} -> {vec_col} vector({dim}) \
             (model={model}, id={id}, backfill={backfill_mode})",
            plan.rel.schema,
            plan.rel.table
        ),
    }
    id
}

/// Register an **existing populated** pgvector column with postvec without
/// recreating or rewriting it. The column's declared `vector(N)` is the
/// authority for the dimension; `model` is a declarative operator assertion
/// that the catalogue can contradict but never prove. `sync => false` adopts
/// in *observed* mode: no DML enqueue triggers, only the TRUNCATE identity
/// sentinel — the rescue path for models postvec cannot embed into.
///
/// Calling `adopt()` again on an active observed entry with `sync => true`
/// and the same vector column and model *promotes* it in place.
#[pg_extern]
#[allow(clippy::too_many_arguments)]
fn adopt(
    relation: &str,
    column_name: &str,
    vector_column: &str,
    model: &str,
    sync: default!(bool, true),
    backfill: default!(&str, "'missing'"),
    backfill_mode: default!(&str, "'queue'"),
    distance: default!(&str, "'cosine'"),
    trigger_mode: default!(&str, "'statement'"),
    fts_config: default!(&str, "'pg_catalog.english'"),
    create_fts_index: default!(bool, false),
    format: default!(Option<String>, "NULL"),
    index_mode: default!(&str, "'manual'"),
    if_not_exists: default!(bool, false),
) -> i64 {
    apply_ddl_lock_timeout();
    let rel = resolve_relation(relation);
    assert_owner(&rel);
    // Pin the relation's definition before any catalogue validation: an
    // ACCESS SHARE lock (held to commit) blocks a concurrent ALTER/DROP
    // TABLE from changing the column facts between validation here and the
    // stronger lock the trigger DDL takes later. Covers promotion too.
    Spi::run(&format!(
        "LOCK TABLE {}.{} IN ACCESS SHARE MODE",
        quote_ident(&rel.schema),
        quote_ident(&rel.table)
    ))
    .unwrap_or_else(|e| error!("postvec: locking the relation for adoption failed: {e}"));

    let backfill = validate_choice("backfill", backfill, &["missing", "all", "none"]);
    let backfill_mode = validate_choice("backfill_mode", backfill_mode, &["queue", "cursor"]);
    let distance = validate_choice("distance", distance, &["cosine", "l2", "ip"]);
    let trigger_mode = validate_choice("trigger_mode", trigger_mode, &["statement", "row"]);
    let index_mode = validate_choice("index_mode", index_mode, &["manual", "immediate", "auto"]);
    if backfill == "all" && backfill_mode == "cursor" {
        // The cursor's chunk filter is `source IS NOT NULL AND vector IS
        // NULL` by construction; a second cursor algorithm is not worth a
        // registry flag and a way to loop forever over rows the worker just
        // filled.
        error!(
            "postvec: backfill => 'all' is not available with backfill_mode => 'cursor' \
             (the cursor's chunk filter skips rows that already have vectors); use \
             backfill_mode => 'queue', or NULL the column first and use backfill => 'missing'"
        );
    }

    // Promotion / already-enabled handling. A stale prior entry (its
    // relation identity gone) is quarantined and adoption proceeds fresh.
    if let Some(prior) = RegistryEntry::load_active(&rel.schema, &rel.table, column_name) {
        let stale = prior.missing_dependency(&[]).or_else(|| {
            prior
                .triggers_missing()
                .then(|| "its triggers are gone (table recreated?)".to_string())
        });
        match stale {
            Some(reason) => quarantine_entry(&prior, &reason),
            None => {
                // Promotion keys on trigger_mode = 'none', not on
                // owns_vector_column: after migration_finalize() the column
                // is owned while the entry is still observed.
                if prior.state == "active"
                    && prior.trigger_mode == "none"
                    && sync
                    && prior.vector_column == vector_column
                    && prior.model == model
                {
                    return promote_observed_entry(
                        &prior,
                        &trigger_mode,
                        &backfill,
                        &backfill_mode,
                        &distance,
                        fts_config,
                        create_fts_index,
                        format.as_deref(),
                        &index_mode,
                    );
                }
                if if_not_exists {
                    return existing_entry_id(
                        &prior,
                        serde_json::json!({
                            "model": model,
                            "vector_column": vector_column,
                            "distance": distance,
                            "trigger_mode": if sync { trigger_mode.as_str() } else { "none" },
                            "index_mode": index_mode,
                            "fts_config": fts_config,
                            "format": format,
                        }),
                    );
                }
                error!(
                    "postvec: {}.{}.{column_name} is already enabled (disable() it first)",
                    rel.schema, rel.table
                );
            }
        }
    }

    let plan = plan_entry(
        rel,
        column_name,
        Some(vector_column.to_string()),
        model,
        fts_config,
        sync && trigger_mode == "statement",
        Origin::Adopted,
        false,
    );
    // On initial adoption a template is also a provenance assertion about
    // every existing non-NULL vector: postvec cannot inspect the text that
    // originally produced them. `backfill => 'all'` re-renders everything;
    // 'missing'/'none' are appropriate only when the stored vectors already
    // follow the declared template.
    let vf = validate_format(
        &plan.rel,
        column_name,
        &plan.vec_col,
        format.as_deref(),
        false,
    );

    // Write-path policy: NOT NULL only for the exact no-worker-write
    // configuration; an embed route whenever anything will be written.
    check_not_null_policy(&plan.rel, &plan.vec_col, plan.vec_not_null, sync, &backfill);
    check_embed_route_policy(model, sync, &backfill);
    // Same informed-consent moment as enable(): adopting onto a
    // provider-backed model means fresh writes (and any backfill) send this
    // column's text to that provider.
    notice_external_provider(model, column_name);
    assert_column_unclaimed(&plan.rel.schema, &plan.rel.table, &plan.vec_col);
    // The advisories only matter when nothing will build the index anyway;
    // immediate/auto get their readiness through ensure_vector_index().
    if index_mode == "manual" {
        warn_index_state(&plan, &distance);
    }
    assert_index_mode_dimension(
        &index_mode,
        plan.dim,
        &format!(
            "{}.{}",
            quote_ident(&plan.rel.schema),
            quote_ident(&plan.rel.table)
        ),
        &plan.vec_col,
        &distance,
    );

    let trigger_mode_store = if sync { trigger_mode.as_str() } else { "none" };
    let backfill_mode_store = if backfill == "none" {
        "none"
    } else {
        backfill_mode.as_str()
    };

    let id = insert_registry_row(
        &plan,
        column_name,
        model,
        fts_config,
        create_fts_index,
        &distance,
        trigger_mode_store,
        backfill_mode_store,
        false,
        vf.format.as_deref(),
        &index_mode,
        None,
    );
    let entry = RegistryEntry::load(id).expect("registry entry just inserted");

    if sync {
        create_triggers(&entry, &trigger_mode);
    } else {
        // Observed: only the relation-identity sentinel; no DML enqueue path.
        create_truncate_sentinel(&entry);
    }

    let qtable = entry.qualified_table();
    if create_fts_index {
        create_fts_index_for(id, &qtable, column_name, fts_config);
    }

    // 'immediate': readiness-first synchronous build. An adopted column that
    // already carries a usable expected-opclass index satisfies it without
    // a duplicate build or any ownership claim. 'manual' is the default
    // and the non-manual modes never duplicate or claim an existing index.
    if index_mode == "immediate" {
        ensure_vector_index(&entry);
    }

    // Backfill fills the gaps, not the column: 'missing' enqueues only
    // NULL-vector rows; 'all' re-embeds every row with source text; 'none'
    // and cursor mode enqueue nothing here (the worker feeds cursor chunks,
    // whose chunk filter is 'missing' by construction).
    if backfill_mode_store == "queue" {
        enqueue_gap_backfill(&entry, backfill == "missing");
    }
    crate::worker::worker_kick();

    log!(
        "postvec: adopted {}.{}.{column_name} -> existing {vector_column} vector({dim}) \
         (model={model}, id={id}, sync={sync}, backfill={backfill}). Note: the model is an \
         operator assertion — a matching dimension is necessary but not sufficient; a wrong \
         assertion makes search() embed queries in the wrong space and migrate(strategy => \
         'convert') convert meaningless inputs",
        entry.table_schema,
        entry.table_name,
        dim = entry.dim,
    );
    id
}

/// Enqueue an adopt-time backfill: every row with source text, optionally
/// restricted to rows whose vector is still NULL.
fn enqueue_gap_backfill(entry: &RegistryEntry, missing_only: bool) {
    // Same one-shot-enqueue scale guard as enable()'s queue backfill: refuse
    // rather than enqueue millions of rows under this verb's held locks.
    assert_queue_backfill_scale(&entry.qualified_table());
    let gap = if missing_only {
        format!(" AND {} IS NULL", quote_ident(&entry.vector_column))
    } else {
        String::new()
    };
    with_key_format(|| {
        Spi::run(&format!(
            "INSERT INTO postvec.jobs (registry_id, pk_value)
         SELECT {id}, {pk} FROM {qtable} WHERE {col} IS NOT NULL{gap}
         ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING",
            id = entry.id,
            pk = entry.pk_text_expr(""),
            qtable = entry.qualified_table(),
            col = quote_ident(&entry.source_column),
        ))
    })
    .unwrap_or_else(|e| error!("postvec: backfill enqueue failed: {e}"));
}

/// The NOT NULL write contract: a NOT NULL vector column is allowed
/// only when **no** worker write is possible — `sync => false` and
/// `backfill => 'none'`. Sync triggers obviously enqueue work, but a finite
/// backfill does too, and a row's source can become NULL between selection
/// and the worker's read; the NULL-job path then executes `UPDATE ... SET
/// vector = NULL`, aborts against the constraint, consumes retries, and
/// dead-letters.
fn check_not_null_policy(rel: &RelInfo, vec_col: &str, not_null: bool, sync: bool, backfill: &str) {
    if not_null && (sync || backfill != "none") {
        error!(
            "postvec: {vec_col:?} on {}.{} is NOT NULL, and this configuration lets the \
             worker write it (a row whose source text becomes NULL gets its vector set NULL, \
             aborting against the constraint and dead-lettering the job). Either run\n  \
             ALTER TABLE {qtbl} ALTER COLUMN {qcol} DROP NOT NULL;\n\
             or adopt read-only with sync => false, backfill => 'none'",
            rel.schema,
            rel.table,
            qtbl = format!("{}.{}", quote_ident(&rel.schema), quote_ident(&rel.table)),
            qcol = quote_ident(vec_col),
        );
    }
}

/// The external provider serving `model`, if any: the `provider` extra of
/// the route `postvec._route()` picks for it.
pub(crate) fn external_provider_of(model: &str) -> Option<String> {
    crate::api::embed::lookup_route(model, None)
        .ok()
        .flatten()
        .and_then(|r| r.provider)
}

/// The external provider serving converter `name`, if any — the convert
/// sibling of [`external_provider_of`]. Matched on the converter's own name:
/// `resolve_convert` returns names, and a name has exactly one owner in the
/// cache.
pub(crate) fn external_provider_of_converter(name: &str) -> Option<String> {
    Spi::get_one_with_args::<String>(
        "SELECT raw->'extra'->>'provider' FROM postvec.models
          WHERE model_type = 'convert' AND name = $1
          LIMIT 1",
        &[name.into()],
    )
    .ok()
    .flatten()
}

/// Notice when a column becomes bound to a provider-backed model: from
/// now on its source text leaves the database host for that provider.
///
/// Emitted by all three verbs that bind a column to a model — `enable`,
/// `adopt` and `migrate`. Migration is the one that is easy to overlook and
/// the one with the largest consequence: `strategy => 'reembed'` sends the
/// *entire existing corpus* to the provider, and even `'convert'` leaves the
/// column sending every future write there. `provider add`'s acknowledgement
/// gate cannot cover it, because by then the provider is already configured.
pub(crate) fn notice_external_provider(model: &str, source_column: &str) {
    if let Some(provider) = external_provider_of(model) {
        pgrx::notice!(
            "postvec: model {model:?} is served by external provider {provider:?}; source \
             text from column {source_column:?} will be sent to that provider for embedding"
        );
    }
}

/// Embed-route policy: `sync => true` always needs an embed route;
/// `backfill => 'missing'|'all'` needs one for the finite repair; a known
/// model without a route is accepted only for the observed no-backfill
/// configuration, with a warning.
fn check_embed_route_policy(model: &str, sync: bool, backfill: &str) {
    match crate::api::embed::resolve_embed_route(model) {
        Ok(_) => {}
        Err(e) => {
            if sync {
                error!(
                    "postvec: {e}; adopt with sync => false (and backfill => 'none') for a \
                     model postvec cannot embed into"
                );
            }
            if backfill != "none" {
                error!(
                    "postvec: {e}; a finite backfill needs an embed route — use \
                     backfill => 'none'"
                );
            }
            warning!(
                "postvec: model {model:?} has no embed route; the entry is observed-only — \
                 search() degrades to FTS and fresh writes are not embedded until a \
                 migration moves the column to an embeddable model"
            );
        }
    }
}

/// Adopt-time index advisories, both warnings: no ANN index at all,
/// or ANN indexes whose opclass does not match the requested distance (the
/// index would silently never serve `search()`).
fn warn_index_state(plan: &EntryPlan, distance: &str) {
    let qtable = format!(
        "{}.{}",
        quote_ident(&plan.rel.schema),
        quote_ident(&plan.rel.table)
    );
    if !crate::registry::vector_index_exists(&qtable, &plan.vec_col) {
        warning!(
            "postvec: {vec:?} on {}.{} has no ANN index; search() will sequential-scan — \
             build one with postvec.create_vector_index() after adoption",
            plan.rel.schema,
            plan.rel.table,
            vec = plan.vec_col,
        );
        return;
    }
    let expected = crate::registry::expected_ann_opclass(distance, plan.dim);
    let opclasses = ann_index_opclasses(plan.rel.oid, &plan.vec_col);
    if !opclasses.is_empty() && !opclasses.iter().any(|oc| oc == expected) {
        warning!(
            "postvec: the ANN index on {vec:?} uses opclass {found:?} but distance \
             {distance:?} on vector({dim}) needs {expected:?}; search() will not use it",
            vec = plan.vec_col,
            found = opclasses.join(", "),
            dim = plan.dim,
        );
    }
}

/// Opclasses of every ANN (hnsw/ivfflat) index that depends on the column —
/// direct key columns *and* expression indexes, matched through the same
/// per-column `pg_depend` edges as [`crate::registry::vector_index_probe_sql`]
/// (never expression-text matching, so a sibling `<col>_new` index cannot
/// false-positive).
pub(crate) fn ann_index_opclasses(rel_oid: pg_sys::Oid, col: &str) -> Vec<String> {
    Spi::connect(|c| {
        let t = c
            .select(
                "SELECT DISTINCT oc.opcname::text
                   FROM pg_index i
                   JOIN pg_class ic ON ic.oid = i.indexrelid
                   JOIN pg_am am ON am.oid = ic.relam
                   JOIN pg_opclass oc ON oc.oid = ANY(i.indclass)
                  WHERE i.indrelid = $1
                    AND am.amname IN ('hnsw', 'ivfflat')
                    AND i.indisvalid AND i.indisready AND i.indislive
                    AND (
                        EXISTS (
                            SELECT 1 FROM pg_attribute a
                             WHERE a.attrelid = i.indrelid
                               AND a.attnum = ANY(i.indkey)
                               AND a.attname = $2
                        )
                        OR EXISTS (
                            SELECT 1
                              FROM pg_depend d
                              JOIN pg_attribute a
                                ON a.attrelid = i.indrelid
                               AND a.attnum = d.refobjsubid
                             WHERE d.classid = 'pg_class'::regclass
                               AND d.objid = i.indexrelid
                               AND d.refclassid = 'pg_class'::regclass
                               AND d.refobjid = i.indrelid
                               AND a.attname = $2
                        )
                    )
                  ORDER BY 1",
                None,
                &[rel_oid.into(), col.into()],
            )
            .unwrap();
        t.into_iter()
            .map(|r| r.get::<String>(1).unwrap().unwrap())
            .collect()
    })
}

/// Promote an active observed entry to synced mode in place: rerun
/// the **whole** read-only adopted-column contract against the live catalog
/// (the table may have been altered since adoption), refuse any change to the
/// entry's immutable options, add only the missing DML enqueue triggers (the
/// sentinel already exists), update `trigger_mode`, and enqueue the requested
/// backfill. Registry id and `owns_vector_column` are preserved.
#[allow(clippy::too_many_arguments)]
fn promote_observed_entry(
    prior: &RegistryEntry,
    trigger_mode: &str,
    backfill: &str,
    backfill_mode: &str,
    distance: &str,
    fts_config: &str,
    create_fts_index: bool,
    format: Option<&str>,
    index_mode: &str,
) -> i64 {
    let rel = resolve_relation(&prior.qualified_table());

    if index_mode != prior.index_mode {
        error!(
            "postvec: promotion changes only sync/trigger_mode/backfill; this entry's \
             index_mode is {stored:?} (requested {index_mode:?}) — disable() and adopt() \
             again to change it",
            stored = prior.index_mode,
        );
    }

    // Immutable adoption options cannot change through promotion — silently
    // keeping the stored value while the call requested another would be the
    // worst outcome. distance/fts_config compare against the registry row
    // (fts normalized through ::regconfig, matching how it is stored).
    // The format compares the exact stored bytes (`$body` vs `${body}` is a
    // difference): set_format() is the only mutation path, because a template
    // change is a declared full refresh, which promotion is not.
    if format != prior.format.as_deref() {
        error!(
            "postvec: promotion changes only sync/trigger_mode/backfill; this entry's \
             format is {stored:?} (requested {format:?}) — a repeated adopt() must supply \
             the exact stored template, and postvec.set_format() is the only way to change \
             it (that change is a declared full refresh)",
            stored = prior.format.as_deref(),
        );
    }
    if distance != prior.distance {
        error!(
            "postvec: promotion changes only sync/trigger_mode/backfill; this entry's \
             distance is {stored:?} (requested {distance:?}) — disable() and adopt() again \
             to change it",
            stored = prior.distance,
        );
    }
    let req_fts =
        Spi::get_one_with_args::<String>("SELECT $1::regconfig::text", &[fts_config.into()])
            .ok()
            .flatten()
            .unwrap_or_else(|| {
                error!("postvec: {fts_config:?} is not a valid text search configuration")
            });
    if req_fts != prior.fts_config {
        error!(
            "postvec: promotion changes only sync/trigger_mode/backfill; this entry's \
             fts_config is {stored:?} (requested {req_fts:?}) — disable() and adopt() again \
             to change it",
            stored = prior.fts_config,
        );
    }
    let stored_cfi = Spi::get_one_with_args::<bool>(
        "SELECT create_fts_index FROM postvec.registry WHERE id = $1",
        &[prior.id.into()],
    )
    .unwrap()
    .unwrap_or(false);
    if create_fts_index != stored_cfi {
        error!(
            "postvec: promotion changes only sync/trigger_mode/backfill; create_fts_index \
             was {stored_cfi} at adoption — disable() and adopt() again to change it"
        );
    }

    // Re-run the read-only adopted-column contract: the column must still be
    // pgvector's exact vector(N), non-generated, with the declared dimension
    // both catalogue-consistent and equal to what the registry records, and
    // the primary key must be the one the enqueue triggers will be generated
    // for. Anything else means the table drifted since adoption.
    let info = vector_column_info(&rel, &prior.vector_column);
    if info.generated {
        error!(
            "postvec: {vec:?} on {tbl} is now a generated column; the worker must write it",
            vec = prior.vector_column,
            tbl = prior.qualified_table(),
        );
    }
    let dim = adopt_dim(&rel, &prior.vector_column, &prior.model, info.dim);
    if dim != prior.dim {
        error!(
            "postvec: {vec:?} on {tbl} is now vector({dim}) but the entry records \
             vector({stored}) — the column was altered since adoption; disable() and \
             adopt() again",
            vec = prior.vector_column,
            tbl = prior.qualified_table(),
            stored = prior.dim,
        );
    }
    // Names AND types: the enqueue triggers key rows by the recorded columns,
    // and the worker casts queued keys back through the recorded pk_types
    // (jobs.rs::dead_letter_uncastable_keys, pk_any_clause). A PK column
    // retyped since adoption (bigint -> text) would pass a name-only check
    // and then dead-letter every non-numeric key at claim time.
    let pk_live = primary_key(&rel);
    let pk_now: Vec<String> = pk_live.iter().map(|(c, _, _)| c.clone()).collect();
    let pk_types_now: Vec<String> = pk_live.iter().map(|(_, t, _)| t.clone()).collect();
    if pk_now != prior.pk_columns || pk_types_now != prior.pk_types {
        error!(
            "postvec: the primary key of {tbl} changed since adoption \
             ({stored:?} {stored_t:?} -> {pk_now:?} {pk_types_now:?}); disable() and \
             adopt() again",
            tbl = prior.qualified_table(),
            stored = prior.pk_columns,
            stored_t = prior.pk_types,
        );
    }
    // Promotion changes the write contract: recheck NOT NULL and the embed
    // route with the requested synced configuration before creating any
    // trigger or job. Do not special-case the observed state that was
    // accepted earlier.
    check_not_null_policy(&rel, &prior.vector_column, info.not_null, true, backfill);
    if let Err(e) = crate::api::embed::resolve_embed_route_for(&prior.model, prior.space.as_deref())
    {
        error!("postvec: {e}; promotion to sync => true requires an embed route");
    }

    create_enqueue_triggers(prior, trigger_mode);
    Spi::run_with_args(
        "UPDATE postvec.registry SET trigger_mode = $2 WHERE id = $1",
        &[prior.id.into(), trigger_mode.into()],
    )
    .unwrap();

    match (backfill, backfill_mode) {
        ("none", _) => {}
        (_, "cursor") => {
            Spi::run_with_args(
                "UPDATE postvec.registry
                    SET backfill_mode = 'cursor', backfill_watermark = NULL
                  WHERE id = $1",
                &[prior.id.into()],
            )
            .unwrap();
        }
        _ => enqueue_gap_backfill(prior, backfill == "missing"),
    }
    crate::worker::worker_kick();

    log!(
        "postvec: promoted observed entry {} ({}.{}.{}) to sync (trigger_mode={trigger_mode}, \
         backfill={backfill})",
        prior.id,
        prior.table_schema,
        prior.table_name,
        prior.source_column
    );
    prior.id
}

/// Change (or clear, with NULL) an entry's document-embedding template.
/// A template change is a declared full refresh: in one transaction it
/// validates the new template, updates the registry, replaces the enqueue
/// triggers (synced entries) and enqueues every table row. There is no
/// "change now, remember to re-embed later" mode.
///
/// The explicit SHARE ROW EXCLUSIVE lock (partition descendants included) is
/// taken even for observed entries, which have no triggers to replace:
/// without it, a row written between the all-row enqueue below and commit
/// could miss the finite refresh. It is held through commit, at the
/// acknowledged cost of blocking table writes while a potentially large
/// enqueue statement runs.
///
/// No template-version column is needed for in-flight worker convergence:
/// an already-claimed old-template job is outside the pending dedup
/// predicate, so this refresh inserts a new pending job for the same PK that
/// the single worker applies *after* the claimed one; an unclaimed old job
/// simply reads the new template when claimed.
#[pg_extern]
fn set_format(relation: &str, column_name: &str, format: Option<&str>) {
    apply_ddl_lock_timeout();
    let rel = resolve_relation(relation);
    assert_owner(&rel);
    Spi::run(&format!(
        "LOCK TABLE {}.{} IN SHARE ROW EXCLUSIVE MODE",
        quote_ident(&rel.schema),
        quote_ident(&rel.table)
    ))
    .unwrap_or_else(|e| error!("postvec: locking the relation for set_format failed: {e}"));

    // Load after the lock, so the entry facts cannot drift under us.
    let entry =
        RegistryEntry::load_active(&rel.schema, &rel.table, column_name).unwrap_or_else(|| {
            error!(
                "postvec: {}.{}.{column_name} is not enabled",
                rel.schema, rel.table
            )
        });
    if entry.state != "active" {
        error!(
            "postvec: {}.{}.{column_name} is in state {:?}; set_format() requires an active \
             entry (finalize or abort the migration first)",
            rel.schema, rel.table, entry.state
        );
    }
    if let Some(reason) = entry.missing_dependency(&[]) {
        error!("postvec: set_format() refused: {reason}");
    }
    if entry.triggers_missing() {
        error!(
            "postvec: {}.{}.{column_name}'s generated triggers are gone (table recreated?); \
             re-run postvec.enable()/adopt() first",
            rel.schema, rel.table
        );
    }

    let vf = validate_format(
        &rel,
        column_name,
        &entry.vector_column,
        format,
        entry.is_recursive(),
    );
    // No-op when the new value IS NOT DISTINCT FROM the stored one. The
    // comparison is bytewise — there is no canonical template serializer, so
    // `$body` -> `${body}` deliberately causes the normal full refresh.
    if vf.format.as_deref() == entry.format.as_deref() {
        log!(
            "postvec: set_format on {}.{}.{column_name} is a no-op (template unchanged)",
            rel.schema,
            rel.table
        );
        return;
    }

    // The finite refresh writes every row (the worker-write policy): it
    // needs an embed route, and a NOT NULL vector column would abort the
    // NULL-source refresh jobs that clear stale vectors.
    if let Err(e) = crate::api::embed::resolve_embed_route_for(&entry.model, entry.space.as_deref())
    {
        error!(
            "postvec: {e}; set_format()'s full refresh re-embeds every row and requires an \
             embed route"
        );
    }
    if !entry.is_recursive() {
        // A chunked entry's vector column lives on the postvec-created
        // destination and is nullable by construction; the NOT NULL policy
        // check applies to (possibly adopted) source-side columns only.
        let not_null = Spi::get_one_with_args::<bool>(
            "SELECT attnotnull FROM pg_attribute
              WHERE attrelid = $1 AND attname = $2 AND attnum > 0 AND NOT attisdropped",
            &[rel.oid.into(), entry.vector_column.as_str().into()],
        )
        .unwrap()
        .unwrap_or(false);
        check_not_null_policy(&rel, &entry.vector_column, not_null, false, "all");
    }

    // Lock order: source (taken above, SHARE ROW EXCLUSIVE) -> destination
    // -> registry row. The destination lock covers the atomic chunk purge
    // below against concurrent worker write-backs.
    if entry.is_recursive() {
        Spi::run(&format!(
            "LOCK TABLE {} IN SHARE ROW EXCLUSIVE MODE",
            entry.qualified_vector_table()
        ))
        .unwrap_or_else(|e| error!("postvec: locking the destination for set_format failed: {e}"));
    }

    Spi::run_with_args(
        "UPDATE postvec.registry SET format = $2 WHERE id = $1",
        &[entry.id.into(), vf.format.as_deref().into()],
    )
    .unwrap_or_else(|e| error!("postvec: storing the format failed: {e}"));

    let entry = RegistryEntry::load(entry.id).expect("entry just updated");
    if entry.trigger_mode != "none" {
        // Synced entries: replace only the enqueue triggers/functions (the
        // TRUNCATE sentinel is untouched) so the change set follows the new
        // referenced-column list. Observed entries have none to replace.
        // Recursive entries regenerate their four chunk triggers — the
        // statement-mode UPDATE trigger carries the referenced columns as
        // arguments, so the change set follows the new template.
        drop_enqueue_triggers(&entry);
        if entry.is_recursive() {
            create_chunk_triggers(&entry, &entry.trigger_mode);
        } else {
            create_enqueue_triggers(&entry, &entry.trigger_mode);
        }
    }
    if entry.is_recursive() {
        // A chunked template change is an atomic full refresh: delete
        // every chunk (the old-template vectors must never mix with
        // new-template ones), all current queue work, and all dead rows; then
        // one refresh per non-NULL source row. Search has a documented
        // empty/partial window while the refreshes drain.
        if queue_backfill_over_limit(&entry.qualified_table()) {
            warning!(
                "postvec: recursive set_format on a table of more than \
                 {QUEUE_BACKFILL_MAX_ROWS} rows purges the destination and enqueues a \
                 refresh per document while this verb's write-blocking table locks are \
                 held; schedule it in a maintenance window"
            );
        }
        Spi::run(&format!("DELETE FROM {}", entry.qualified_vector_table()))
            .unwrap_or_else(|e| error!("postvec: purging chunks for set_format failed: {e}"));
        Spi::run_with_args(
            "DELETE FROM postvec.jobs
              WHERE registry_id = $1
                AND NOT (op = 'refresh' AND claimed_at IS NOT NULL)",
            &[entry.id.into()],
        )
        .unwrap();
        Spi::run_with_args(
            "DELETE FROM postvec.jobs_dead WHERE registry_id = $1",
            &[entry.id.into()],
        )
        .unwrap();
        with_key_format(|| {
            Spi::run(&format!(
                "INSERT INTO postvec.jobs (registry_id, pk_value, op)
             SELECT {id}, {pk}, 'refresh' FROM {qtable} WHERE {col} IS NOT NULL
             ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING",
                id = entry.id,
                pk = entry.pk_text_expr(""),
                qtable = entry.qualified_table(),
                col = quote_ident(&entry.source_column),
            ))
        })
        .unwrap_or_else(|e| error!("postvec: set_format refresh enqueue failed: {e}"));
    } else {
        enqueue_full_refresh(&entry);
    }
    crate::worker::worker_kick();

    log!(
        "postvec: set_format on {}.{}.{column_name}: {desc}; every row was enqueued for \
         re-embedding (search sees a mix of old- and new-template vectors until the \
         refresh drains)",
        entry.table_schema,
        entry.table_name,
        desc = match entry.format.as_deref() {
            Some(f) => format!("template set to {f:?}"),
            None => "template cleared (raw source column)".to_string(),
        },
    );
}

/// Drop exactly the enqueue trigger set (ins/upd/pk triggers and functions),
/// leaving the TRUNCATE sentinel in place — `set_format()`'s replacement
/// boundary.
fn drop_enqueue_triggers(entry: &RegistryEntry) {
    let id = entry.id;
    let qtable = entry.qualified_table();
    let mut ddl = String::new();
    // "del" exists only on recursive entries; IF EXISTS keeps one list.
    for trg in ["ins", "upd", "pk", "del"] {
        ddl.push_str(&format!(
            "DROP TRIGGER IF EXISTS {name} ON {qtable};\n",
            name = quote_ident(&format!("postvec_{trg}_{id}")),
        ));
    }
    ddl.push_str(&format!(
        "DROP FUNCTION IF EXISTS postvec.trg_ins_{id}();\n\
         DROP FUNCTION IF EXISTS postvec.trg_upd_{id}();\n\
         DROP FUNCTION IF EXISTS postvec.trg_pk_{id}();",
    ));
    Spi::run(&ddl).unwrap_or_else(|e| error!("postvec: replacing enqueue triggers failed: {e}"));
}

/// The dedicated all-rows refresh: enqueue EVERY row, **including NULL-source
/// rows** — for those, the refresh job is what converges any pre-existing/
/// adopted vector to NULL. Deliberately not a flag on
/// [`enqueue_gap_backfill`], whose existing contract intentionally skips NULL
/// sources.
fn enqueue_full_refresh(entry: &RegistryEntry) {
    // set_format() has no worker-paced alternative (the refresh must be
    // atomic with the trigger swap), so a large table WARNS rather than
    // refuses: the operator should schedule the call in a quiet window —
    // the SHARE ROW EXCLUSIVE lock blocks all writes until it commits.
    if queue_backfill_over_limit(&entry.qualified_table()) {
        warning!(
            "postvec: enqueueing a full refresh of more than {QUEUE_BACKFILL_MAX_ROWS} \
             rows while this verb's write-blocking table lock is held; on a table this \
             size, schedule set_format() in a maintenance window"
        );
    }
    with_key_format(|| {
        Spi::run(&format!(
            "INSERT INTO postvec.jobs (registry_id, pk_value)
         SELECT {id}, {pk} FROM {qtable}
         ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING",
            id = entry.id,
            pk = entry.pk_text_expr(""),
            qtable = entry.qualified_table(),
        ))
    })
    .unwrap_or_else(|e| error!("postvec: full-refresh enqueue failed: {e}"));
}

/// The GIN FTS index over the source column (shared by `enable()`/`adopt()`).
fn create_fts_index_for(id: i64, qtable: &str, column_name: &str, fts_config: &str) {
    Spi::run(&format!(
        "CREATE INDEX {idx} ON {qtable} USING gin (to_tsvector({cfg}::regconfig, {col}::text))",
        idx = quote_ident(&format!("postvec_fts_{id}")),
        cfg = quote_literal(fts_config),
        col = quote_ident(column_name),
    ))
    .unwrap_or_else(|e| error!("postvec: FTS index creation failed: {e}"));
    mark_index_depends_on_extension(qtable, &format!("postvec_fts_{id}"), column_name);
}

/// GIN FTS index over destination `chunk_text`. The recursive entry's
/// lexical leg searches chunk text, not the source column. Same
/// conventional name and extension-dependency stamp as the column-mode index.
pub(crate) fn create_chunk_fts_index_for(entry: &RegistryEntry) {
    let id = entry.id;
    let qdest = entry.qualified_vector_table();
    Spi::run(&format!(
        "CREATE INDEX {idx} ON {qdest}
              USING gin (to_tsvector({cfg}::regconfig, chunk_text))",
        idx = quote_ident(&format!("postvec_fts_{id}")),
        cfg = quote_literal(&entry.fts_config),
    ))
    .unwrap_or_else(|e| error!("postvec: chunk FTS index creation failed: {e}"));
    mark_index_depends_on_extension(&qdest, &format!("postvec_fts_{id}"), "chunk_text");
}

/// Generate the per-entry INSERT/UPDATE trigger pair and the shared TRUNCATE
/// trigger. Function/trigger names are mode-independent so `disable()` tears
/// them down uniformly; only the bodies/definitions differ.
///
/// - `statement` (default): statement-level triggers with transition tables —
///   one firing per statement, set-based enqueue (bulk-load coalescing). The
///   trade-off is the UPDATE trigger fires for *every* update (no `OF col`
///   column list is allowed with transition tables) and joins the transition
///   tables to find changed rows.
/// - `row`: row-level triggers with an `AFTER UPDATE OF col ... WHEN
///   (new IS DISTINCT FROM old)` clause — fires only on real changes to the
///   column, at the cost of per-row (not coalesced) enqueues.
///
/// Both modes get the row-level PK-change companion trigger (`trg_pk_<id>`):
/// a PK-mutating update re-keys the row so a pending job for the old key
/// cannot strand the text change unembedded.
fn create_triggers(entry: &RegistryEntry, mode: &str) {
    create_enqueue_triggers(entry, mode);
    create_truncate_sentinel(entry);
}

/// The INSERT/UPDATE/PK-companion enqueue triggers and functions — the DML
/// write path. Observed (`sync => false`) adoption never calls this; promotion
/// calls only this (the sentinel already exists).
///
/// The UPDATE change set is derived from the entry's referenced columns:
/// exactly `[source_column]` with no template (the generated DDL then
/// matches the no-template shape byte for byte), the template's full
/// referenced set otherwise, so a change to any context column re-enqueues
/// the row. INSERT stays gated on the source column alone: a NULL source
/// means a NULL vector regardless of context.
fn create_enqueue_triggers(entry: &RegistryEntry, mode: &str) {
    let id = entry.id;
    let qtable = entry.qualified_table();
    let col = quote_ident(&entry.source_column);
    let refs = entry
        .referenced_columns()
        .unwrap_or_else(|e| error!("postvec: stored format template is invalid: {e}"));
    // Statement mode compares old/new transition rows per referenced column;
    // row mode uses the same columns in AFTER UPDATE OF and its WHEN guard.
    let stmt_change_pred = refs
        .iter()
        .map(|c| {
            let q = quote_ident(c);
            format!("n.{q} IS DISTINCT FROM o.{q}")
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let row_of_list = refs
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");
    let row_when_pred = refs
        .iter()
        .map(|c| {
            let q = quote_ident(c);
            format!("NEW.{q} IS DISTINCT FROM OLD.{q}")
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let ins_trg = quote_ident(&format!("postvec_ins_{id}"));
    let upd_trg = quote_ident(&format!("postvec_upd_{id}"));
    let pk_trg = quote_ident(&format!("postvec_pk_{id}"));
    // Text key expressions: NEW.-qualified for row triggers, transition-table
    // aliased for statement triggers. Composite PKs key as ROW(...)::text.
    let pk_new = entry.pk_text_expr("NEW");
    let pk_n = entry.pk_text_expr("n");
    let keys = postvec_core::registry::key_settings_clause();
    // Transition-table join on every PK column (composite-safe).
    let pk_join = entry
        .pk_columns
        .iter()
        .map(|c| format!("o.{q} = n.{q}", q = quote_ident(c)))
        .collect::<Vec<_>>()
        .join(" AND ");
    let pk_changed = entry
        .pk_columns
        .iter()
        .map(|c| {
            let q = quote_ident(c);
            format!("NEW.{q} IS DISTINCT FROM OLD.{q}")
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    let pk_update_cols = entry
        .pk_columns
        .iter()
        .map(|c| quote_ident(c))
        .collect::<Vec<_>>()
        .join(", ");

    let ins_upd = if mode == "row" {
        format!(
            r#"
CREATE FUNCTION postvec.trg_ins_{id}() RETURNS trigger LANGUAGE plpgsql {keys} AS $pv$
BEGIN
    IF NEW.{col} IS NOT NULL THEN
        INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ({id}, {pk_new})
        ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING;
        PERFORM postvec.worker_kick();
    END IF;
    RETURN NULL;
END $pv$;
CREATE TRIGGER {ins_trg} AFTER INSERT ON {qtable}
    FOR EACH ROW EXECUTE FUNCTION postvec.trg_ins_{id}();

CREATE FUNCTION postvec.trg_upd_{id}() RETURNS trigger LANGUAGE plpgsql {keys} AS $pv$
BEGIN
    INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ({id}, {pk_new})
    ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $pv$;
CREATE TRIGGER {upd_trg} AFTER UPDATE OF {row_of_list} ON {qtable}
    FOR EACH ROW WHEN ({row_when_pred})
    EXECUTE FUNCTION postvec.trg_upd_{id}();

-- PK-change companion (same as statement mode): a pending job is keyed by
-- the OLD pk text; an update that changes the PK without touching {col}
-- would otherwise leave that job pointing at a nonexistent row and the text
-- change silently never embedded.
CREATE FUNCTION postvec.trg_pk_{id}() RETURNS trigger LANGUAGE plpgsql {keys} AS $pv$
BEGIN
    INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ({id}, {pk_new})
    ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $pv$;
CREATE TRIGGER {pk_trg} AFTER UPDATE OF {pk_update_cols} ON {qtable}
    FOR EACH ROW WHEN ({pk_changed})
    EXECUTE FUNCTION postvec.trg_pk_{id}();
"#
        )
    } else {
        format!(
            r#"
CREATE FUNCTION postvec.trg_ins_{id}() RETURNS trigger LANGUAGE plpgsql {keys} AS $pv$
BEGIN
    INSERT INTO postvec.jobs (registry_id, pk_value)
    SELECT {id}, {pk_n} FROM new_table n WHERE n.{col} IS NOT NULL
    ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $pv$;
CREATE TRIGGER {ins_trg} AFTER INSERT ON {qtable}
    REFERENCING NEW TABLE AS new_table
    FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_ins_{id}();

CREATE FUNCTION postvec.trg_upd_{id}() RETURNS trigger LANGUAGE plpgsql {keys} AS $pv$
BEGIN
    INSERT INTO postvec.jobs (registry_id, pk_value)
    SELECT {id}, {pk_n}
      FROM new_table n JOIN old_table o ON {pk_join}
     WHERE {stmt_change_pred}
    ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $pv$;
-- No `OF {col}` column list — Postgres forbids column lists with transition
-- tables. The body's `IS DISTINCT FROM` guard restricts enqueues to real changes.
CREATE TRIGGER {upd_trg} AFTER UPDATE ON {qtable}
    REFERENCING OLD TABLE AS old_table NEW TABLE AS new_table
    FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_upd_{id}();

CREATE FUNCTION postvec.trg_pk_{id}() RETURNS trigger LANGUAGE plpgsql {keys} AS $pv$
BEGIN
    INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ({id}, {pk_new})
    ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $pv$;
CREATE TRIGGER {pk_trg} AFTER UPDATE OF {pk_update_cols} ON {qtable}
    FOR EACH ROW WHEN ({pk_changed})
    EXECUTE FUNCTION postvec.trg_pk_{id}();
"#
        )
    };

    Spi::run(&ins_upd).unwrap_or_else(|e| error!("postvec: trigger creation failed: {e}"));
    mark_enqueue_objects_depend_on_extension(entry);
}

/// The shared `postvec.trg_truncate` TRUNCATE trigger. For synced entries it
/// purges pending jobs on TRUNCATE; for observed entries it is additionally
/// the low-cost relation-identity sentinel: its absence is how a
/// dropped-and-recreated same-named table is detected. TRUNCATE is
/// inherently statement-level in every mode.
fn create_truncate_sentinel(entry: &RegistryEntry) {
    let id = entry.id;
    let qtable = entry.qualified_table();
    let trunc_trg = quote_ident(&format!("postvec_trunc_{id}"));
    let ddl = format!(
        "CREATE TRIGGER {trunc_trg} AFTER TRUNCATE ON {qtable}
    FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_truncate('{id}');"
    );
    Spi::run(&ddl).unwrap_or_else(|e| error!("postvec: TRUNCATE trigger creation failed: {e}"));
    let dep = format!("ALTER TRIGGER {trunc_trg} ON {qtable} DEPENDS ON EXTENSION postvec;");
    if let Err(e) = Spi::run(&dep) {
        warn_dependency_marking_failed(&e);
    }
}

// Split on the same enqueue/sentinel boundary as trigger creation: promotion
// re-marks only the enqueue objects it just created.
fn mark_enqueue_objects_depend_on_extension(entry: &RegistryEntry) {
    let id = entry.id;
    let qtable = entry.qualified_table();
    let ddl = format!(
        "ALTER FUNCTION postvec.trg_ins_{id}() DEPENDS ON EXTENSION postvec;\n\
         ALTER FUNCTION postvec.trg_upd_{id}() DEPENDS ON EXTENSION postvec;\n\
         ALTER FUNCTION postvec.trg_pk_{id}() DEPENDS ON EXTENSION postvec;\n\
         ALTER TRIGGER {ins} ON {qtable} DEPENDS ON EXTENSION postvec;\n\
         ALTER TRIGGER {upd} ON {qtable} DEPENDS ON EXTENSION postvec;\n\
         ALTER TRIGGER {pk} ON {qtable} DEPENDS ON EXTENSION postvec;",
        ins = quote_ident(&format!("postvec_ins_{id}")),
        upd = quote_ident(&format!("postvec_upd_{id}")),
        pk = quote_ident(&format!("postvec_pk_{id}")),
    );
    if let Err(e) = Spi::run(&ddl) {
        warn_dependency_marking_failed(&e);
    }
}

fn warn_dependency_marking_failed(e: &pgrx::spi::SpiError) {
    warning!(
        "postvec: could not mark generated triggers as extension-dependent; \
         run postvec.uninstall() before DROP EXTENSION to remove them cleanly: {e}"
    );
}

/// The conventional index name resolved in `qtable`'s schema (where postvec
/// creates its indexes), or `None` when the table itself is gone.
fn qualified_index_name(qtable: &str, index_name: &str) -> Option<String> {
    let schema = Spi::get_one_with_args::<String>(
        "SELECT n.nspname::text
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
          WHERE c.oid = to_regclass($1)",
        &[qtable.into()],
    )
    .ok()
    .flatten()?;
    Some(format!(
        "{}.{}",
        quote_ident(&schema),
        quote_ident(index_name)
    ))
}

/// Is `qidx` **this entry's** index? `None` = no such relation. `Some(true)`
/// only when it is an index on `qtable` that depends on `column` (direct key
/// or expression edge) — and, when `require_extension_dep`, additionally
/// carries the extension-dependency edge stamped at creation. Everything
/// postvec decides about a conventionally-named index goes through this one
/// predicate: index names are only unique per schema, so the *name* proves
/// nothing about the table it serves.
fn index_is_entrys(
    qidx: &str,
    qtable: &str,
    column: &str,
    require_extension_dep: bool,
) -> Option<bool> {
    Spi::get_one_with_args::<bool>(
        "SELECT i.indrelid = to_regclass($2)
                AND (
                    EXISTS (
                        SELECT 1 FROM pg_attribute a
                         WHERE a.attrelid = i.indrelid
                           AND a.attnum = ANY(i.indkey)
                           AND a.attname = $3
                    )
                    OR EXISTS (
                        SELECT 1
                          FROM pg_depend d
                          JOIN pg_attribute a
                            ON a.attrelid = i.indrelid
                           AND a.attnum = d.refobjsubid
                         WHERE d.classid = 'pg_class'::regclass
                           AND d.objid = i.indexrelid
                           AND d.refclassid = 'pg_class'::regclass
                           AND d.refobjid = i.indrelid
                           AND a.attname = $3
                    )
                )
                AND (NOT $4 OR EXISTS (
                    SELECT 1 FROM pg_depend d
                     JOIN pg_extension e ON e.oid = d.refobjid
                    WHERE d.classid = 'pg_class'::regclass
                      AND d.objid = c.oid
                      AND d.refclassid = 'pg_extension'::regclass
                      AND d.deptype = 'x'
                      AND e.extname = 'postvec'))
           FROM pg_class c
           JOIN pg_index i ON i.indexrelid = c.oid
          WHERE c.oid = to_regclass($1)",
        &[
            qidx.into(),
            qtable.into(),
            column.into(),
            require_extension_dep.into(),
        ],
    )
    .ok()
    .flatten()
}

/// Stamp a postvec-generated index with the extension-dependency edge that
/// [`drop_owned_index`] later requires as proof of ownership — but only after
/// verifying the name resolves to an index **on `qtable` depending on
/// `column`**. Index names are schema-unique, so an unrelated table's
/// same-named index could otherwise be claimed here and then silently dropped
/// by `DROP EXTENSION postvec`. Anything unverified is skipped silently
/// (e.g. the operator built a differently-named index).
pub(crate) fn mark_index_depends_on_extension(qtable: &str, index_name: &str, column: &str) {
    let Some(qidx) = qualified_index_name(qtable, index_name) else {
        return;
    };
    if index_is_entrys(&qidx, qtable, column, false) != Some(true) {
        return;
    }
    if let Err(e) = Spi::run(&format!("ALTER INDEX {qidx} DEPENDS ON EXTENSION postvec")) {
        warning!(
            "postvec: could not mark index {qidx} as extension-dependent; teardown will \
             leave it in place: {e}"
        );
    }
}

/// `immediate`/`auto` run ordinary HNSW builds, which pgvector caps at
/// 2000 dimensions. Above that only the manual halfvec expression index
/// works, so both non-manual modes refuse up front (before any DDL or
/// registry write) with the exact manual suggestion.
pub(crate) fn assert_index_mode_dimension(
    index_mode: &str,
    dim: i32,
    qtable: &str,
    vec_col: &str,
    distance: &str,
) {
    if index_mode != "manual" && dim > 2000 {
        error!(
            "postvec: index_mode {index_mode:?} cannot build an HNSW index over {dim} \
             dimensions (pgvector caps vector_*_ops at 2000); keep index_mode 'manual' and \
             build the halfvec expression index yourself:\n  \
             CREATE INDEX CONCURRENTLY ON {qtable} USING hnsw \
             (({vec}::halfvec({dim})) {opclass});",
            vec = quote_ident(vec_col),
            opclass = crate::registry::halfvec_opclass(distance),
        );
    }
}

/// Thin readiness predicate, composing two catalog facts:
/// [`crate::registry::expected_ann_opclass`] (what `search()` will actually
/// use for this distance/dimension) and [`ann_index_opclasses`] (the
/// opclasses of every valid, ready, live ANN index covering the vector
/// column, through direct-key or expression dependency edges). True iff
/// one usable index carries the expected opclass. Says nothing about
/// ownership: a valid operator-built index satisfies readiness without
/// ever being stamped or claimed.
pub(crate) fn ann_index_ready(entry: &RegistryEntry) -> bool {
    // The ANN index lives on the entry's vector target: the destination
    // for a recursive entry.
    let rel_oid = Spi::get_one_with_args::<pg_sys::Oid>(
        "SELECT to_regclass($1)::oid",
        &[entry.qualified_vector_table().as_str().into()],
    )
    .ok()
    .flatten()
    .unwrap_or(pg_sys::Oid::INVALID);
    if rel_oid == pg_sys::Oid::INVALID {
        return false;
    }
    let expected = crate::registry::expected_ann_opclass(&entry.distance, entry.dim);
    ann_index_opclasses(rel_oid, &entry.vector_column)
        .iter()
        .any(|oc| oc == expected)
}

/// The readiness-first wrapper shared by `index_mode => 'immediate'`, the
/// worker's auto reconciliation, and the public [`create_vector_index`]: if
/// any usable expected-opclass index already exists — operator- or
/// postvec-owned — succeed without creating, renaming, or stamping anything;
/// otherwise build the conventional-name index (ownership-safe tri-state in
/// [`build_vector_index`]). Success clears `index_error` while preserving
/// `index_mode`, which is what gives an operator a clean recovery path after
/// a parked auto failure.
pub(crate) fn ensure_vector_index(entry: &RegistryEntry) {
    if !ann_index_ready(entry) {
        build_vector_index(
            entry.id,
            &entry.qualified_vector_table(),
            &entry.vector_column,
            entry.dim,
            &entry.distance,
        );
    }
    Spi::run_with_args(
        "UPDATE postvec.registry SET index_error = NULL WHERE id = $1",
        &[entry.id.into()],
    )
    .unwrap_or_else(|e| error!("postvec: clearing index_error failed: {e}"));
}

/// Build the pgvector index for an entry, opclass chosen from its distance
/// metric. Idempotent for postvec's own index; a same-named index postvec
/// cannot prove it created is a **name collision and refuses** — the previous
/// `IF NOT EXISTS` would silently skip creation and then claim whatever the
/// name resolved to. Refuses HNSW above pgvector's 2000-dim limit with a
/// halfvec hint.
pub(crate) fn build_vector_index(id: i64, qtable: &str, vec_col: &str, dim: i32, distance: &str) {
    if dim > 2000 {
        error!(
            "postvec: HNSW indexes support up to 2000 dimensions (got {dim}); build a \
             halfvec expression index manually for this column"
        );
    }
    let name = format!("postvec_vec_{id}");
    let qidx = qualified_index_name(qtable, &name)
        .unwrap_or_else(|| error!("postvec: relation {qtable} vanished during index build"));
    match index_is_entrys(&qidx, qtable, vec_col, true) {
        None => {
            Spi::run(&format!(
                "CREATE INDEX {idx} ON {qtable} USING hnsw ({vec} {opclass})",
                idx = quote_ident(&name),
                vec = quote_ident(vec_col),
                opclass = crate::registry::distance_opclass(distance),
            ))
            .unwrap_or_else(|e| error!("postvec: vector index creation failed: {e}"));
            mark_index_depends_on_extension(qtable, &name, vec_col);
        }
        Some(true) => {} // postvec's own index from an earlier call: idempotent
        Some(false) => error!(
            "postvec: an object named {qidx} already exists and postvec cannot prove it \
             created it (wrong table/column or missing extension dependency); rename or \
             drop it — search() does not require postvec's index name, any ANN index on \
             the column works"
        ),
    }
}

/// Build (or confirm) the pgvector index for an enabled column — call after a
/// backfill drains (index-after-load is faster and less bloated). Idempotent,
/// and readiness-first: an existing valid expected-opclass index (yours or
/// postvec's) satisfies the call without a duplicate build. Success clears a
/// parked `index_error`, preserving the entry's `index_mode`. This is the
/// documented repair path after a failed automatic build.
#[pg_extern]
fn create_vector_index(relation: &str, column_name: &str) {
    apply_ddl_lock_timeout();
    let rel = resolve_relation(relation);
    assert_owner(&rel);
    let entry =
        RegistryEntry::load_active(&rel.schema, &rel.table, column_name).unwrap_or_else(|| {
            error!(
                "postvec: {}.{}.{column_name} is not enabled",
                rel.schema, rel.table
            )
        });
    ensure_vector_index(&entry);
    log!(
        "postvec: vector index verified/built for {}.{}.{column_name} ({} distance)",
        rel.schema,
        rel.table,
        entry.distance
    );
}

/// `disable()` reached for an entry whose source relation no longer exists
/// (for example a prior `DROP TABLE ... CASCADE` removed the source with
/// the generated view/policy, quarantining the entry). The registry row
/// still holds the destination and its ownership token. This path
/// authorizes against the destination's owner and performs only queue
/// purge, state change and, with the flag, the proven destination
/// teardown.
fn disable_orphaned_entry(relation: &str, column_name: &str, drop_destination: bool) {
    let (schema_pat, table_pat) = match relation.rsplit_once('.') {
        Some((s, t)) => (Some(s.to_string()), t.to_string()),
        None => (None, relation.to_string()),
    };
    let entry: Option<RegistryEntry> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT table_schema, table_name FROM postvec.registry
                  WHERE table_name = $1 AND ($2::text IS NULL OR table_schema = $2)
                    AND source_column = $3",
                None,
                &[
                    table_pat.as_str().into(),
                    schema_pat.as_deref().into(),
                    column_name.into(),
                ],
            )
            .ok()?;
        let names: Vec<(String, String)> = t
            .into_iter()
            .map(|r| {
                (
                    r.get::<String>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap().unwrap(),
                )
            })
            .collect();
        match names.as_slice() {
            [(s, t)] => RegistryEntry::load_any(s, t, column_name),
            _ => None,
        }
    });
    let Some(entry) = entry else {
        error!("postvec: relation {relation:?} does not exist");
    };
    if !entry.is_recursive() {
        error!(
            "postvec: relation {relation:?} does not exist (its registry entry needs no \
             further teardown; re-enable on a new table clears it)"
        );
    }
    // Authorization: the destination's owner (the source owner at enable
    // time) or a superuser.
    let owner_ok = Spi::get_one_with_args::<bool>(
        "SELECT pg_catalog.pg_has_role(current_user, c.relowner, 'USAGE')
           FROM pg_class c WHERE c.oid = to_regclass($1)",
        &[entry.qualified_vector_table().as_str().into()],
    )
    .ok()
    .flatten()
    .unwrap_or(unsafe { pg_sys::superuser() });
    if !owner_ok {
        error!(
            "postvec: must own {} (or be a superuser) to tear it down",
            entry.qualified_vector_table()
        );
    }
    if drop_destination {
        if let Err(reason) = lock_and_prove_destination(&entry) {
            error!(
                "postvec: refusing to drop the destination for the vanished source \
                 {relation:?}: {reason}"
            );
        }
    }
    Spi::run_with_args(
        "DELETE FROM postvec.jobs WHERE registry_id = $1",
        &[entry.id.into()],
    )
    .unwrap();
    Spi::run_with_args(
        "UPDATE postvec.registry SET state = 'disabled' WHERE id = $1",
        &[entry.id.into()],
    )
    .unwrap();
    if drop_destination {
        drop_destination_objects(&entry);
    } else {
        log!(
            "postvec: source {relation:?} is gone; the destination {} is retained — remove \
             it with drop_destination => true",
            entry.qualified_vector_table()
        );
    }
}

#[pg_extern]
fn disable(
    relation: &str,
    column_name: &str,
    drop_column: default!(bool, false),
    drop_destination: default!(bool, false),
) {
    apply_ddl_lock_timeout();
    // A vanished source with a retained destination still needs a teardown
    // verb; route it through the registry instead of failing on name
    // resolution ([R2-11]).
    let missing =
        Spi::get_one_with_args::<bool>("SELECT to_regclass($1) IS NULL", &[relation.into()])
            .unwrap()
            .unwrap_or(true);
    if missing {
        return disable_orphaned_entry(relation, column_name, drop_destination);
    }
    let rel = resolve_relation(relation);
    assert_owner(&rel);

    // Idempotent: find the entry in any state so an already-disabled entry
    // can still have its shadow column dropped via drop_column — or, for a
    // recursive entry, its retained destination removed via drop_destination
    // Disable first, reclaim the space later, without reaching for the
    // fleet-wide uninstall().
    let entry =
        RegistryEntry::load_any(&rel.schema, &rel.table, column_name).unwrap_or_else(|| {
            error!(
                "postvec: {}.{}.{column_name} is not enabled",
                rel.schema, rel.table
            )
        });
    if entry.state == "migrating" {
        error!(
            "postvec: {}.{}.{column_name} has a migration in progress; finalize or abort it \
             first (see postvec.migration_status())",
            rel.schema, rel.table
        );
    }
    // Each destructive flag belongs to exactly one mode; the wrong one is
    // an error, never a silent no-op.
    if drop_column && entry.is_recursive() {
        error!(
            "postvec: this is a chunked entry — there is no source vector column to drop; \
             use drop_destination => true to remove the managed destination table and view"
        );
    }
    if drop_destination && !entry.is_recursive() {
        error!(
            "postvec: drop_destination applies only to chunked entries; use drop_column for \
             a column-mode entry"
        );
    }
    // Ownership guard: postvec never drops a column it did not create.
    // Refusing beats warning. An explicit flag plus a WARNING in a psql
    // scrollback is not consent for irreversible data loss.
    if drop_column && !entry.owns_vector_column {
        error!(
            "postvec: {vec:?} on {tbl} was adopted, not created by postvec — it will not be \
             dropped. Run disable() without drop_column, then \
             ALTER TABLE {tbl} DROP COLUMN {qvec}; yourself if that is what you mean",
            vec = entry.vector_column,
            tbl = entry.qualified_table(),
            qvec = quote_ident(&entry.vector_column),
        );
    }
    // Destructive destination teardown requires the positive ownership proof,
    // taken UNDER the source → destination → view ACCESS EXCLUSIVE locks —
    // BEFORE any other effect, so a refusal leaves a still-enabled entry
    // untouched and no concurrent swap can slip between proof and DROP.
    if drop_destination {
        if let Err(reason) = lock_and_prove_destination(&entry) {
            error!(
                "postvec: refusing to drop the destination for {}.{}.{column_name}: {reason}",
                rel.schema, rel.table
            );
        }
    }
    let id = entry.id;
    let qtable = format!("{}.{}", quote_ident(&rel.schema), quote_ident(&rel.table));

    teardown_entry_objects(&entry, true);

    Spi::run_with_args(
        "DELETE FROM postvec.jobs WHERE registry_id = $1",
        &[id.into()],
    )
    .unwrap();
    Spi::run_with_args(
        "UPDATE postvec.registry SET state = 'disabled' WHERE id = $1",
        &[id.into()],
    )
    .unwrap();

    if drop_column {
        Spi::run(&format!(
            "ALTER TABLE {qtable} DROP COLUMN IF EXISTS {vec}",
            vec = quote_ident(&entry.vector_column),
        ))
        .unwrap_or_else(|e| error!("postvec: DROP COLUMN failed: {e}"));
    }
    if drop_destination {
        drop_destination_objects(&entry);
    } else if entry.is_recursive() {
        log!(
            "postvec: the destination {dest} and its view are retained with their data \
             (frozen; postvec no longer maintains them). Remove them later with \
             postvec.disable({rel_lit}, {col_lit}, drop_destination => true)",
            dest = entry.qualified_vector_table(),
            rel_lit = quote_literal(relation),
            col_lit = quote_literal(column_name),
        );
    }

    log!(
        "postvec: disabled {}.{}.{column_name} (id={id})",
        rel.schema,
        rel.table
    );
}

/// Remove all runtime-created postvec objects from enabled tables. This is a
/// pre-uninstall escape hatch: run it before `DROP EXTENSION postvec` if any
/// columns may still be enabled. By default it leaves shadow vector columns in
/// place; pass `drop_columns => true` to remove them too.
#[pg_extern]
fn uninstall(drop_columns: default!(bool, false), drop_destinations: default!(bool, false)) -> i64 {
    apply_ddl_lock_timeout();
    // Unlike enable()/disable() (per-table ownership), this sweeps every
    // registry entry in the database — gate it explicitly instead of relying
    // on the raw DDL to fail midway on tables the caller doesn't own.
    if !unsafe { pg_sys::superuser() } {
        error!("postvec: uninstall() requires a superuser (it tears down every enabled entry)");
    }
    let ids: Vec<i64> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT id FROM postvec.registry ORDER BY id DESC",
                None,
                &[],
            )
            .unwrap_or_else(|e| error!("postvec: uninstall registry scan failed: {e}"));
        t.into_iter()
            .map(|r| r.get::<i64>(1).unwrap().unwrap())
            .collect()
    });

    let mut cleaned = 0i64;
    for id in ids {
        let Some(entry) = RegistryEntry::load(id) else {
            continue;
        };
        let rel_exists = Spi::get_one_with_args::<bool>(
            "SELECT to_regclass($1) IS NOT NULL",
            &[entry.qualified_table().as_str().into()],
        )
        .unwrap_or(Some(false))
        .unwrap_or(false);
        // Destructive destination teardown proves ownership under the
        // source -> destination locks, taken before any other per-entry
        // effect so the acquisition order matches every lifecycle path and
        // nothing can swap the proven objects before the later DROP. A
        // failed proof retains the destination with the reason. The sweep
        // must finish either way.
        let destination_proof: Option<Result<(), String>> =
            if entry.is_recursive() && drop_destinations {
                Some(lock_and_prove_destination(&entry))
            } else {
                None
            };
        teardown_entry_objects(&entry, rel_exists);
        Spi::run_with_args(
            "DELETE FROM postvec.jobs WHERE registry_id = $1",
            &[id.into()],
        )
        .unwrap_or_else(|e| error!("postvec: uninstall job purge failed: {e}"));
        // Capture live migration scratch columns BEFORE the state rewrite
        // below turns them into 'aborted': only migrations in a state that
        // can still physically own the `_new` name (running /
        // awaiting_finalize / failed) are postvec's to clean. After done /
        // aborted / awaiting_index the name is absent or already renamed, and
        // the application may legitimately reuse it. Dropping a same-named
        // future user column would violate the ownership invariant.
        let scratch_cols: Vec<String> = Spi::connect(|c| {
            let t = c
                .select(
                    "SELECT DISTINCT new_column FROM postvec.migrations
                      WHERE registry_id = $1
                        AND state IN ('running','awaiting_finalize','failed')",
                    None,
                    &[id.into()],
                )
                .unwrap_or_else(|e| error!("postvec: uninstall migration column scan failed: {e}"));
            t.into_iter()
                .filter_map(|r| r.get::<String>(1).ok().flatten())
                .collect()
        });
        Spi::run_with_args(
            "UPDATE postvec.migrations
                SET state = CASE
                        WHEN state IN ('done','aborted') THEN state
                        ELSE 'aborted'
                    END,
                    finished_at = COALESCE(finished_at, now()),
                    error = COALESCE(error, 'postvec.uninstall()')
              WHERE registry_id = $1",
            &[id.into()],
        )
        .unwrap_or_else(|e| error!("postvec: uninstall migration update failed: {e}"));

        if entry.is_recursive() {
            // Same preserve-by-default rule as disable(); the proof above
            // already holds the locks, so the drop here cannot hit a swap.
            if drop_destinations {
                match destination_proof {
                    Some(Ok(())) => drop_destination_objects(&entry),
                    Some(Err(reason)) => warning!(
                        "postvec: uninstall keeps the destination for {}.{}.{}: {reason}",
                        entry.table_schema,
                        entry.table_name,
                        entry.source_column,
                    ),
                    None => unreachable!("proof gathered for every recursive entry"),
                }
            } else {
                log!(
                    "postvec: uninstall retains destination {} and its view (frozen data); \
                     pass drop_destinations => true to remove proven postvec destinations",
                    entry.qualified_vector_table(),
                );
            }
        } else if drop_columns && rel_exists {
            let mut cols = if entry.owns_vector_column {
                vec![entry.vector_column.clone()]
            } else {
                log!(
                    "postvec: keeping adopted column {} on {} (postvec did not create it)",
                    entry.vector_column,
                    entry.qualified_table()
                );
                vec![]
            };
            cols.extend(scratch_cols);
            cols.sort();
            cols.dedup();
            for col in cols {
                Spi::run(&format!(
                    "ALTER TABLE {tbl} DROP COLUMN IF EXISTS {col}",
                    tbl = entry.qualified_table(),
                    col = quote_ident(&col),
                ))
                .unwrap_or_else(|e| error!("postvec: uninstall column drop failed: {e}"));
            }
        }

        Spi::run_with_args(
            "UPDATE postvec.registry SET state = 'disabled' WHERE id = $1",
            &[id.into()],
        )
        .unwrap_or_else(|e| error!("postvec: uninstall registry update failed: {e}"));
        cleaned += 1;
    }
    cleaned
}

/// Safe dead-letter re-drive. Consumes dead rows for one entry (all of
/// them, or an explicit id selection) and inserts fresh pending jobs, in
/// the caller's transaction. Returns the number of dead rows consumed,
/// not the number of queue rows inserted: several dead rows can share a
/// PK, and a PK can already have a pending job. The existing partial
/// unique index remains the dedup authority.
///
/// This is the one narrow exception to the management functions'
/// invoker-rights shape: `postvec.jobs_dead` grants PUBLIC only SELECT, so the
/// delete must run as the extension owner. The function is therefore
/// `SECURITY DEFINER` with `search_path` pinned to `pg_catalog, pg_temp`,
/// every postvec object is schema-qualified, and authorization inspects the
/// **invoking** role via `GetOuterUserId()` — `current_user` would be the
/// function owner in here, while the outer user id respects a legitimate
/// `SET ROLE` and ignores the definer switch.
///
/// The `regclass` parameter is deliberate: the caller's session resolves
/// `'docs'` to an OID under *its* search path before the definer context (and
/// its pinned path) is entered, so unqualified calls keep working and an
/// attacker-controlled search path cannot redirect any object reference.
#[pg_extern(security_definer)]
#[search_path(pg_catalog, pg_temp)]
fn retry_dead(
    relation: pgrx::PgRelation,
    column_name: &str,
    dead_ids: default!(Option<Vec<Option<i64>>>, "NULL"),
) -> i64 {
    apply_ddl_lock_timeout();
    let rel_oid = relation.oid();
    let invoker = unsafe { pg_sys::GetOuterUserId() };

    // Relation facts by OID (never by name — the name already did its job in
    // the caller's regclass conversion): kind, identity, and whether the
    // invoker owns / is a member of the owning role, the same rule the other
    // management verbs apply through assert_owner().
    let facts: Option<(String, String, String, bool)> = Spi::connect(|c| {
        let t = c
            .select(
                "SELECT n.nspname::text, c.relname::text, c.relkind::text,
                        pg_catalog.pg_has_role($2, c.relowner, 'USAGE')
                   FROM pg_catalog.pg_class c
                   JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  WHERE c.oid = $1",
                Some(1),
                &[rel_oid.into(), invoker.into()],
            )
            .unwrap();
        t.into_iter().next().map(|r| {
            (
                r.get::<String>(1).unwrap().unwrap(),
                r.get::<String>(2).unwrap().unwrap(),
                r.get::<String>(3).unwrap().unwrap(),
                r.get::<bool>(4).unwrap().unwrap_or(false),
            )
        })
    });
    let Some((schema, table, relkind, owner_ok)) = facts else {
        error!("postvec: relation vanished during retry_dead()");
    };
    if relkind != "r" && relkind != "p" {
        error!(
            "postvec: {schema}.{table} is not an ordinary or partitioned table \
             (relkind={relkind})"
        );
    }
    if !owner_ok {
        error!(
            "postvec: must own {schema}.{table} (or be a member of its owning role) to \
             re-drive its dead jobs"
        );
    }

    let entry = RegistryEntry::load_any(&schema, &table, column_name).unwrap_or_else(|| {
        error!("postvec: {schema}.{table}.{column_name} is not enabled");
    });
    if entry.state != "active" {
        error!(
            "postvec: {schema}.{table}.{column_name} is in state {:?}; only active entries \
             can re-drive dead jobs (finalize/abort a migration, or re-enable, first)",
            entry.state
        );
    }
    if let Some(reason) = entry.missing_dependency(&[]) {
        error!(
            "postvec: {schema}.{table}.{column_name} cannot be re-driven: {reason}; \
             re-run postvec.enable()/adopt() on the current table first"
        );
    }
    if entry.triggers_missing() {
        error!(
            "postvec: {schema}.{table}.{column_name}'s generated triggers are gone \
             (table recreated?); re-run postvec.enable()/adopt() first"
        );
    }

    // Explicit ids are validated before anything is consumed: prove every id
    // exists and belongs to this entry (a mixed list must never consume — or
    // even reference — another entry's rows).
    let picked_ids: Option<Vec<i64>> = match dead_ids {
        None => None,
        Some(ids) => {
            if ids.is_empty() {
                error!(
                    "postvec: dead_ids is empty; pass NULL (or omit it) to re-drive every \
                     dead job for the entry"
                );
            }
            if ids.iter().any(Option::is_none) {
                error!("postvec: dead_ids contains a NULL element");
            }
            let mut ids: Vec<i64> = ids.into_iter().flatten().collect();
            ids.sort_unstable();
            ids.dedup();

            let owned: Vec<i64> = Spi::connect(|c| {
                let t = c
                    .select(
                        "SELECT dead_id FROM postvec.jobs_dead
                          WHERE dead_id = ANY($1) AND registry_id = $2",
                        None,
                        &[ids.clone().into(), entry.id.into()],
                    )
                    .unwrap();
                t.into_iter()
                    .map(|r| r.get::<i64>(1).unwrap().unwrap())
                    .collect()
            });
            if owned.len() != ids.len() {
                let bad: Vec<i64> = ids.iter().filter(|i| !owned.contains(i)).copied().collect();
                error!(
                    "postvec: dead id(s) {bad:?} do not exist or belong to another entry; \
                     nothing was re-driven (see postvec.jobs_dead)"
                );
            }
            Some(ids)
        }
    };

    // Consume + re-drive in one set-based statement per mode: the dead rows
    // are deleted and fresh pending jobs inserted without ever materializing
    // the dead queue in memory. A row-by-row shape would hold every dead
    // row as a Rust vector and, for chunked entries, run one liveness
    // probe per chunk. Attempts/error/claim state reset naturally, never
    // copied from the dead rows. Distinct identities (several dead rows
    // can share one) coalesce with any already-pending job through the
    // partial unique index.
    //
    // The unfiltered form is bounded per call so one invocation cannot turn a
    // multi-million-row dead queue into one giant transaction; a NOTICE says
    // to call again when a full batch was consumed.
    //
    // Op/chunk identity is preserved. A dead refresh re-drives as a
    // refresh. A dead child embed re-drives only while its exact
    // (chunk_id, source pk) identity still exists in the destination (an
    // obsolete child is consumed with a NOTICE: its input was replaced, so
    // a fresh refresh already covers the document). A NULL-chunk embed
    // dead row on a chunked entry (a malformed direct queue insert
    // dead-lettered at claim time) can never succeed and is consumed too.
    const RETRY_DEAD_MAX_ROWS: i64 = 100_000;
    let (consumed, obsolete) = with_key_format(|| {
        if entry.is_recursive() {
            let qdest = entry.qualified_vector_table();
            let pk_type = &entry.pk_types[0];
            let q = format!(
                "WITH pick AS (
                 SELECT dead_id FROM postvec.jobs_dead
                  WHERE registry_id = $1 AND ($2::int8[] IS NULL OR dead_id = ANY($2))
                  ORDER BY dead_id
                  LIMIT $3
             ), del AS (
                 DELETE FROM postvec.jobs_dead d USING pick p
                  WHERE d.dead_id = p.dead_id
                 RETURNING d.pk_value, d.op, d.chunk_id
             ), dedup AS (
                 SELECT DISTINCT pk_value, op, chunk_id FROM del
             ), refresh_ins AS (
                 INSERT INTO postvec.jobs (registry_id, pk_value, op)
                 SELECT $1, pk_value, 'refresh' FROM dedup WHERE op = 'refresh'
                 ON CONFLICT (registry_id, op, pk_value, chunk_id)
                 WHERE claimed_at IS NULL DO NOTHING
             ), embed_ins AS (
                 INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id)
                 SELECT $1, d.pk_value, 'embed', d.chunk_id
                   FROM dedup d
                   JOIN {qdest} c ON c.postvec_chunk_id = d.chunk_id
                                 AND c.postvec_source_pk = d.pk_value::{pk_type}
                  WHERE d.op = 'embed' AND d.chunk_id IS NOT NULL
                 ON CONFLICT (registry_id, op, pk_value, chunk_id)
                 WHERE claimed_at IS NULL DO NOTHING
             )
             SELECT (SELECT count(*) FROM del),
                    (SELECT count(*) FROM dedup d
                      WHERE d.op <> 'refresh'
                        AND (d.chunk_id IS NULL OR NOT EXISTS (
                             SELECT 1 FROM {qdest} c
                              WHERE c.postvec_chunk_id = d.chunk_id
                                AND c.postvec_source_pk = d.pk_value::{pk_type})))"
            );
            Spi::connect_mut(|c| {
                let t = c
                    .update(
                        q.as_str(),
                        None,
                        &[
                            entry.id.into(),
                            picked_ids.clone().into(),
                            RETRY_DEAD_MAX_ROWS.into(),
                        ],
                    )
                    .unwrap_or_else(|e| error!("postvec: re-driving dead jobs failed: {e}"));
                let row = t.into_iter().next();
                let row = row.as_ref();
                (
                    row.and_then(|r| r.get::<i64>(1).unwrap()).unwrap_or(0),
                    row.and_then(|r| r.get::<i64>(2).unwrap()).unwrap_or(0),
                )
            })
        } else {
            let consumed = Spi::connect_mut(|c| {
                let t = c
                    .update(
                        "WITH pick AS (
                         SELECT dead_id FROM postvec.jobs_dead
                          WHERE registry_id = $1
                            AND ($2::int8[] IS NULL OR dead_id = ANY($2))
                          ORDER BY dead_id
                          LIMIT $3
                     ), del AS (
                         DELETE FROM postvec.jobs_dead d USING pick p
                          WHERE d.dead_id = p.dead_id
                         RETURNING d.pk_value
                     ), ins AS (
                         INSERT INTO postvec.jobs (registry_id, pk_value)
                         SELECT DISTINCT $1, pk_value FROM del
                         ON CONFLICT (registry_id, op, pk_value, chunk_id)
                         WHERE claimed_at IS NULL DO NOTHING
                     )
                     SELECT count(*) FROM del",
                        None,
                        &[
                            entry.id.into(),
                            picked_ids.clone().into(),
                            RETRY_DEAD_MAX_ROWS.into(),
                        ],
                    )
                    .unwrap_or_else(|e| error!("postvec: re-driving dead jobs failed: {e}"));
                t.into_iter()
                    .next()
                    .and_then(|r| r.get::<i64>(1).unwrap())
                    .unwrap_or(0)
            });
            (consumed, 0i64)
        }
    });

    if let Some(ids) = &picked_ids {
        if consumed != ids.len() as i64 {
            // The whole transaction rolls back: partial consumption of an
            // explicit list must never commit.
            error!(
                "postvec: {} of {} requested dead id(s) no longer exist (a concurrent \
                 retry_dead() consumed them); nothing was re-driven",
                ids.len() as i64 - consumed,
                ids.len()
            );
        }
    } else if consumed == RETRY_DEAD_MAX_ROWS {
        pgrx::notice!(
            "postvec: consumed a full batch of {RETRY_DEAD_MAX_ROWS} dead job(s); more may \
             remain — call postvec.retry_dead() again"
        );
    }
    if obsolete > 0 {
        pgrx::notice!(
            "postvec: {obsolete} dead chunk job(s) were consumed as obsolete — their \
             chunk identity no longer exists (the document was refreshed since)"
        );
    }
    if consumed == 0 {
        return 0;
    }
    crate::worker::worker_kick();

    log!("postvec: re-drove {consumed} dead job(s) for {schema}.{table}.{column_name}");
    consumed
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use crate::registry::RegistryEntryDb as _;
    use pgrx::prelude::*;

    /// Seed a fake embed model so enable() resolves a dimension without network.
    fn seed_model(name: &str, dim: i32) {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ($1, 'embed', $1, $2, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
            &[name.into(), dim.into()],
        )
        .unwrap();
    }

    fn make_docs() {
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
    }

    // ---- Recursive chunking: schema / API surface ----

    fn enable_recursive_docs() -> i64 {
        seed_model("m", 4);
        make_docs();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks')",
        )
        .unwrap()
        .unwrap()
    }

    #[pg_test]
    fn recursive_enable_creates_destination_view_rls_and_marker() {
        let id = enable_recursive_docs();

        // Registry shape.
        let row = Spi::get_one_with_args::<bool>(
            "SELECT chunking = 'recursive' AND chunk_size = 2000 AND chunk_overlap = 200
                    AND destination_schema = 'public' AND destination_table = 'docs_chunks'
                    AND destination_view = 'docs_chunks_view'
                    AND destination_token IS NOT NULL
               FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap();
        assert_eq!(
            row,
            Some(true),
            "registry carries the whole recursive shape"
        );

        // No vector column was added to the source.
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass AND attname = 'body_semantic'
                    AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "recursive enable adds no source column"
        );

        // Destination shape: identity PK, typed source key, seq/offsets/text,
        // vector, and the (source_pk, seq) unique constraint.
        let dest_cols = Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_attribute
              WHERE attrelid = 'docs_chunks'::regclass AND attnum > 0 AND NOT attisdropped
                AND attname IN ('postvec_chunk_id','postvec_source_pk','postvec_chunk_seq',
                                'postvec_char_start','postvec_char_end','chunk_text',
                                'body_semantic')",
        )
        .unwrap();
        assert_eq!(dest_cols, Some(7));
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs_chunks'::regclass AND attname = 'postvec_source_pk'"
            )
            .unwrap()
            .as_deref(),
            Some("bigint"),
            "source key clones the source PK type"
        );
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs_chunks'::regclass AND attname = 'body_semantic'"
            )
            .unwrap()
            .as_deref(),
            Some("vector(4)")
        );
        // No redundant standalone source-key index [R3-3]: exactly the PK and
        // the two-column unique index exist.
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_index WHERE indrelid = 'docs_chunks'::regclass"
            )
            .unwrap(),
            Some(2),
            "destination has exactly the PK and the (source_pk, seq) unique index"
        );

        // RLS enabled and forced, with the source-visibility policy.
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT relrowsecurity AND relforcerowsecurity FROM pg_class
                  WHERE oid = 'docs_chunks'::regclass"
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_policy WHERE polrelid = 'docs_chunks'::regclass"
            )
            .unwrap(),
            Some(1)
        );

        // View exists with invoker+barrier options and both markers carry the
        // token.
        let view_opts = Spi::get_one::<String>(
            "SELECT array_to_string(reloptions, ',') FROM pg_class
              WHERE oid = 'docs_chunks_view'::regclass",
        )
        .unwrap()
        .unwrap_or_default();
        assert!(
            view_opts.contains("security_invoker=true")
                && view_opts.contains("security_barrier=true"),
            "view options: {view_opts}"
        );
        let token = Spi::get_one_with_args::<String>(
            "SELECT destination_token FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap()
        .unwrap();
        for rel in ["docs_chunks", "docs_chunks_view"] {
            let comment = Spi::get_one_with_args::<String>(
                "SELECT obj_description(to_regclass($1), 'pg_class')",
                &[rel.into()],
            )
            .unwrap()
            .unwrap_or_default();
            assert!(comment.contains(&token), "{rel} comment carries the token");
        }

        // The five source triggers exist (ins/upd/pk/del/trunc).
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'"
            )
            .unwrap(),
            Some(5)
        );
    }

    /// [R2-3]/[R3-2]: the generated view depends on no vector column and no
    /// whole-row source type — an unrelated source column can be dropped and
    /// the (future) migration scratch column added/dropped without touching
    /// the view.
    #[pg_test]
    fn recursive_view_is_lean() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, unrelated text)",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks')",
        )
        .unwrap();
        // Dropping an unrelated source column succeeds without CASCADE.
        Spi::run("ALTER TABLE docs DROP COLUMN unrelated").unwrap();
        // Dropping the destination's vector column is not blocked by the view
        // (proves the view has no vector dependency; the entry would
        // quarantine, but no dependent object exists).
        Spi::run("ALTER TABLE docs_chunks DROP COLUMN body_semantic").unwrap();
        assert!(
            Spi::get_one::<bool>("SELECT EXISTS (SELECT 1 FROM docs_chunks_view)")
                .unwrap()
                .is_some(),
            "view still valid"
        );
    }

    #[pg_test]
    fn recursive_backfill_enqueues_refresh_jobs() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b'), (NULL)").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE registry_id = $1 AND op = 'refresh' AND chunk_id IS NULL",
                &[id.into()],
            )
            .unwrap(),
            Some(2),
            "one refresh per non-NULL source row, no embed jobs"
        );
    }

    /// Refusal matrix for the four recursive-enable arguments. Every refusal
    /// must leave no registry row, destination, view, trigger, or job.
    #[pg_test]
    fn recursive_validation_refusal_matrix() {
        seed_model("m", 4);
        make_docs();
        Spi::run("CREATE TABLE taken (x int)").unwrap();
        for (call, why) in [
            ("chunking => 'recursive'", "destination is required"),
            ("chunk_size => 500", "chunk args without recursive"),
            (
                "chunking => 'recursive', destination => 'a.b'",
                "qualified destination name",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks', chunk_size => 63",
                "chunk_size below 64",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks', chunk_size => 100001",
                "chunk_size above 100000",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks',
                 chunk_size => 100, chunk_overlap => 100",
                "overlap >= size",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks',
                 chunk_overlap => -1",
                "negative overlap",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks',
                 index_mode => 'immediate'",
                "immediate index mode",
            ),
            (
                "chunking => 'recursive', destination => 'taken'",
                "destination name already exists",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks',
                 vector_column => 'chunk_text'",
                "reserved destination column",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks',
                 format => '$body'",
                "recursive template without $chunk",
            ),
            (
                "chunking => 'recursive', destination => 'docs_chunks',
                 format => '$chunk $body'",
                "recursive template referencing the source column",
            ),
        ] {
            let q = format!("SELECT postvec.enable('docs','body','m', {call})");
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "must refuse: {why}");
            assert_eq!(
                Spi::get_one::<i64>("SELECT count(*) FROM postvec.registry").unwrap(),
                Some(0),
                "no registry row after refusal: {why}"
            );
            assert_eq!(
                Spi::get_one::<bool>("SELECT to_regclass('docs_chunks') IS NULL").unwrap(),
                Some(true),
                "no destination after refusal: {why}"
            );
            assert_eq!(
                Spi::get_one::<i64>(
                    "SELECT count(*) FROM pg_trigger
                      WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'"
                )
                .unwrap(),
                Some(0),
                "no trigger after refusal: {why}"
            );
        }

        // Composite PK refusal.
        Spi::run("CREATE TABLE comp (a int, b int, body text, PRIMARY KEY (a, b))").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.enable('comp','body','m', chunking => 'recursive',
                                       destination => 'comp_chunks')",
            )
            .ok();
        });
        assert!(r.is_err(), "composite PK must refuse recursive mode");
    }

    /// Round 7: byte-boundability is classified by base-type OID, never by
    /// display name — arrays (rendered "numeric[]", "timestamp …[]",
    /// "character varying[]"), composites and custom types whose names mimic
    /// allowed prefixes are refused; domains are unwrapped to their base
    /// type; and a post-enable ALTER COLUMN TYPE is caught by the worker's
    /// dependency check (quarantine) instead of rendering unmeasured.
    #[pg_test]
    fn byte_boundability_is_classified_by_base_type_oid() {
        seed_model("m", 4);
        Spi::run("CREATE DOMAIN dom_text AS text").unwrap();
        Spi::run("CREATE DOMAIN dom_jsonb AS jsonb").unwrap();
        Spi::run("CREATE TYPE timestamp_pair AS (a timestamptz, b timestamptz)").unwrap();
        Spi::run(
            "CREATE TABLE tb (id bigint PRIMARY KEY, body text, arr numeric[],
                              ts_arr timestamp[], vch_arr character varying[],
                              pair timestamp_pair, dt dom_text, dj dom_jsonb, j jsonb)",
        )
        .unwrap();

        // Unsafe SOURCE columns are refused outright.
        for (col, why) in [
            ("arr", "numeric[] (display name starts with 'numeric')"),
            (
                "ts_arr",
                "timestamp[] (display name starts with 'timestamp')",
            ),
            ("vch_arr", "character varying[] (starts with 'character')"),
            ("pair", "composite named to mimic 'timestamp'"),
            ("dj", "domain over jsonb (must unwrap, then refuse)"),
            ("j", "jsonb"),
        ] {
            let q = format!("SELECT postvec.enable('tb', '{col}', 'm')");
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "must refuse {why} as a source column");
        }

        // Template CONTEXT columns get the same classification.
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.enable('tb','body','m', format => '$arr $body')")
                .ok();
        });
        assert!(r.is_err(), "must refuse an array context column");

        // A domain over text is admitted — classification unwraps to the
        // base type instead of refusing the unfamiliar display name.
        let id = Spi::get_one::<i64>("SELECT postvec.enable('tb','dt','m', format => '$dt $body')")
            .unwrap();
        assert!(
            id.is_some(),
            "domain-over-text source + text context admits"
        );

        // Post-enable ALTER TYPE on a rendered (context) column: the worker
        // repeats the classification on its dependency check and reports the
        // entry quarantinable rather than trusting the enable-time verdict.
        let entry = crate::registry::RegistryEntry::load_active("public", "tb", "dt")
            .expect("entry exists");
        assert!(
            entry.missing_dependency(&[]).is_none(),
            "healthy before ALTER"
        );
        Spi::run("ALTER TABLE tb ALTER COLUMN body TYPE jsonb USING to_jsonb(body)").unwrap();
        let reason = entry
            .missing_dependency(&[])
            .expect("ALTER COLUMN TYPE must fail the dependency check");
        assert!(reason.contains("byte-boundable"), "reason: {reason}");
    }

    /// [R3-5]: the destination source key clones typmod and non-default
    /// collation exactly.
    #[pg_test]
    fn recursive_destination_clones_pk_typmod_and_collation() {
        seed_model("m", 4);
        Spi::run(r#"CREATE TABLE vdocs (code varchar(16) COLLATE "C" PRIMARY KEY, body text)"#)
            .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('vdocs','body','m', chunking => 'recursive',
                                   destination => 'vdocs_chunks')",
        )
        .unwrap();
        let (typ, coll) = (
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'vdocs_chunks'::regclass
                    AND attname = 'postvec_source_pk'",
            )
            .unwrap(),
            Spi::get_one::<String>(
                "SELECT co.collname::text FROM pg_attribute a
                   JOIN pg_collation co ON co.oid = a.attcollation
                  WHERE a.attrelid = 'vdocs_chunks'::regclass
                    AND a.attname = 'postvec_source_pk'",
            )
            .unwrap(),
        );
        assert_eq!(typ.as_deref(), Some("character varying(16)"));
        assert_eq!(coll.as_deref(), Some("C"), "non-default collation cloned");
    }

    #[pg_test]
    fn recursive_disable_retains_then_drops_destination() {
        let id = enable_recursive_docs();

        // Wrong flag for the mode is an error, not a no-op.
        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.disable('docs','body', drop_column => true)").ok();
        });
        assert!(r.is_err(), "drop_column refused for a chunked entry");

        // Default disable retains destination + view + policy.
        Spi::run("SELECT postvec.disable('docs','body')").unwrap();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT to_regclass('docs_chunks') IS NOT NULL
                        AND to_regclass('docs_chunks_view') IS NOT NULL"
            )
            .unwrap(),
            Some(true),
            "default disable retains the destination tree"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'"
            )
            .unwrap(),
            Some(0),
            "triggers gone"
        );

        // [R2-11]: a re-run on the already-disabled entry with the flag
        // performs only the teardown.
        Spi::run("SELECT postvec.disable('docs','body', drop_destination => true)").unwrap();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT to_regclass('docs_chunks') IS NULL
                        AND to_regclass('docs_chunks_view') IS NULL"
            )
            .unwrap(),
            Some(true),
            "explicit drop removes both objects"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT state FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("disabled")
        );
    }

    #[pg_test]
    fn recursive_drop_destination_requires_exact_marker() {
        enable_recursive_docs();
        // An operator (or restore surgery) clears the comment: automatic
        // teardown is disarmed and refuses; the object survives.
        Spi::run("COMMENT ON TABLE docs_chunks IS 'mine now'").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.disable('docs','body', drop_destination => true)").ok();
        });
        assert!(
            r.is_err(),
            "changed marker must refuse destructive teardown"
        );
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('docs_chunks') IS NOT NULL").unwrap(),
            Some(true),
            "the destination survives the refusal"
        );
    }

    /// The warn-and-continue promise of `uninstall()` must survive the
    /// destination *name* being reused by a non-table relation: `LOCK TABLE`
    /// would raise "cannot lock relation" on a sequence and abort the whole
    /// sweep, so the proof path locks by OID with a relkind preflight instead.
    #[pg_test]
    fn uninstall_warns_past_a_sequence_replaced_destination() {
        enable_recursive_docs();
        Spi::run("DROP VIEW docs_chunks_view").unwrap();
        Spi::run("DROP TABLE docs_chunks").unwrap();
        Spi::run("CREATE SEQUENCE docs_chunks").unwrap();

        // Must complete (warn-and-keep), not abort on the LOCK.
        let cleaned = Spi::get_one::<i64>("SELECT postvec.uninstall(drop_destinations => true)")
            .unwrap()
            .unwrap();
        assert_eq!(cleaned, 1, "the sweep still processes the entry");
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT relkind::text FROM pg_class WHERE relname = 'docs_chunks'"
            )
            .unwrap()
            .as_deref(),
            Some("S"),
            "the impostor sequence is kept, never dropped"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry").unwrap(),
            Some("disabled".into()),
            "the entry itself is still swept to disabled"
        );
    }

    /// Exact source `DROP` dependency behavior: plain DROP is refused
    /// while the generated view/policy exist, CASCADE removes those dependents
    /// but not the independently owned destination, and a later teardown of
    /// the orphaned entry can still prove and drop the marked table.
    #[pg_test]
    fn source_drop_dependency_behavior_is_exact() {
        enable_recursive_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::run("DROP TABLE docs").ok();
        });
        assert!(
            r.is_err(),
            "plain DROP is refused while the generated view/policy depend on the source"
        );
    }

    #[pg_test]
    fn source_drop_cascade_retains_destination_for_later_teardown() {
        enable_recursive_docs();
        Spi::run("DROP TABLE docs CASCADE").unwrap();
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT to_regclass('docs_chunks') IS NOT NULL
                        AND to_regclass('docs_chunks_view') IS NULL"
            )
            .unwrap(),
            Some(true),
            "CASCADE removed the view/policy but not the independently owned table"
        );
        // The orphaned entry can still be torn down by name, proving the
        // destination through its marker alone ([R2-11]).
        Spi::run("SELECT postvec.disable('docs','body', drop_destination => true)").unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT to_regclass('docs_chunks') IS NULL").unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("disabled")
        );
    }

    /// `set_format()` on a chunked entry atomically purges every chunk
    /// and queue row, regenerates the triggers with the new change set, and
    /// enqueues one refresh per non-NULL source row — old- and new-template
    /// chunk vectors never mix.
    #[pg_test]
    fn recursive_set_format_is_an_atomic_full_refresh() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                title text, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', chunking => 'recursive',
                                   destination => 'docs_chunks', backfill => false)",
        )
        .unwrap()
        .unwrap();
        Spi::run("INSERT INTO docs (title, body) VALUES ('t', 'a doc'), ('u', NULL)").unwrap();
        // Materialize + fake-embed one chunk, plus a dead row.
        while crate::worker::chunk::process_one_refresh(5).processed {}
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        Spi::run("UPDATE docs_chunks SET body_semantic = '[1,2,3,4]'::vector").unwrap();
        Spi::run_with_args(
            "INSERT INTO postvec.jobs_dead (job_id, registry_id, pk_value, op) \
             VALUES (0, $1, '1', 'refresh')",
            &[id.into()],
        )
        .unwrap();

        Spi::run("SELECT postvec.set_format('docs','body', '[$title] $chunk')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM docs_chunks").unwrap(),
            Some(0),
            "old-template chunks are purged atomically"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE registry_id = $1 AND op = 'refresh'",
                &[id.into()],
            )
            .unwrap(),
            Some(1),
            "one refresh per non-NULL source row"
        );
        // The statement-mode UPDATE trigger's change set now includes the
        // new context column: a title-only update re-fires.
        while crate::worker::chunk::process_one_refresh(5).processed {}
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        Spi::run("UPDATE docs SET title = 'new title' WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE registry_id = $1 AND op = 'refresh'",
                &[id.into()],
            )
            .unwrap(),
            Some(1),
            "the regenerated triggers watch the template's context columns"
        );
    }

    #[pg_test]
    fn column_mode_refuses_drop_destination() {
        seed_model("m", 4);
        make_docs();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.disable('docs','body', drop_destination => true)").ok();
        });
        assert!(r.is_err(), "drop_destination refused for a column entry");
    }

    /// The active-vector-target unique index: a column-mode row keeps
    /// today's exact key, and a recursive row protects the destination — an
    /// unrelated same-named column on the source is NOT blocked [R3-4].
    #[pg_test]
    fn vector_target_uniqueness_keys_the_physical_target() {
        let _id = enable_recursive_docs();
        // A same-named vector column on the SOURCE is unrelated to the
        // recursive entry's destination target and must be allowed.
        Spi::run("ALTER TABLE docs ADD COLUMN body_semantic vector(4)").unwrap();
        Spi::run(
            "CREATE TABLE docs2 (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        // A second recursive entry on another table with the same
        // vector_column name but a different destination is fine.
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs2','body','m', chunking => 'recursive',
                                   destination => 'docs2_chunks')",
        )
        .unwrap();
        // But two active rows naming the same physical destination target
        // violate the index.
        let r = std::panic::catch_unwind(|| {
            Spi::run(
                "INSERT INTO postvec.registry
                     (table_schema, table_name, source_column, vector_column,
                      pk_columns, pk_types, model, dim, chunking, chunk_size,
                      chunk_overlap, destination_schema, destination_table,
                      destination_view, destination_token)
                 VALUES ('public','other','body','body_semantic',
                         ARRAY['id'], ARRAY['bigint'], 'm', 4, 'recursive', 2000,
                         200, 'public', 'docs_chunks', 'docs_chunks_view', 'tok')",
            )
            .ok();
        });
        assert!(
            r.is_err(),
            "two active writers for one destination vector target must violate the index"
        );
    }

    /// A non-superuser source owner (granted the operator bundle) can
    /// enable a recursive entry, and an ordinary application writer can fire
    /// every shared trigger. An attacker-table registry mismatch is a no-op
    /// warning.
    #[pg_test]
    fn non_superuser_enable_and_application_writes() {
        seed_model("m", 4);
        Spi::run("CREATE ROLE p5_owner LOGIN").unwrap();
        Spi::run("CREATE ROLE p5_app LOGIN").unwrap();
        Spi::run("GRANT ALL ON SCHEMA public TO p5_owner").unwrap();
        // The operator bundle a non-superuser owner needs for enable() itself
        // (SECURITY INVOKER): registry insert/cleanup and the backfill's
        // op-carrying job insert. The *triggers* need none of this — that is
        // the point of the shared SECURITY DEFINER functions.
        Spi::run("GRANT INSERT, DELETE ON postvec.registry TO p5_owner").unwrap();
        Spi::run("GRANT INSERT (registry_id, pk_value, op) ON postvec.jobs TO p5_owner").unwrap();

        Spi::run("SET ROLE p5_owner").unwrap();
        Spi::run(
            "CREATE TABLE odocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('odocs','body','m', chunking => 'recursive',
                                   destination => 'odocs_chunks')",
        )
        .unwrap()
        .unwrap();
        Spi::run("GRANT INSERT, UPDATE, DELETE, SELECT ON odocs TO p5_app").unwrap();
        Spi::run("RESET ROLE").unwrap();

        // The application writer fires INSERT/UPDATE/DELETE without any
        // postvec grant beyond the standing PUBLIC ones.
        Spi::run("SET ROLE p5_app").unwrap();
        Spi::run("INSERT INTO odocs (body) VALUES ('one'), ('two')").unwrap();
        Spi::run("UPDATE odocs SET body = 'one!' WHERE id = 1").unwrap();
        Spi::run("DELETE FROM odocs WHERE id = 2").unwrap();
        Spi::run("RESET ROLE").unwrap();
        // Row 1 has one pending refresh; row 2's work was invalidated by the
        // delete.
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE registry_id = $1 AND op = 'refresh' AND pk_value = '1'",
                &[id.into()],
            )
            .unwrap(),
            Some(1)
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1 AND pk_value = '2'",
                &[id.into()],
            )
            .unwrap(),
            Some(0),
            "the deleted row's queue state is purged"
        );

        // Deputy check: an attacker attaches the shared function to their own
        // table with the victim's registry id — every firing is a warned
        // no-op and touches nothing of the victim's.
        Spi::run("GRANT CREATE ON SCHEMA public TO p5_app").unwrap();
        Spi::run("SET ROLE p5_app").unwrap();
        Spi::run("CREATE TABLE attacker (id bigint PRIMARY KEY, body text)").unwrap();
        Spi::run(&format!(
            "CREATE TRIGGER att AFTER INSERT ON attacker
                 REFERENCING NEW TABLE AS new_table
                 FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_chunk_ins('{id}')"
        ))
        .unwrap();
        Spi::run("INSERT INTO attacker VALUES (99, 'x')").unwrap();
        Spi::run("RESET ROLE").unwrap();
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1 AND pk_value = '99'",
                &[id.into()],
            )
            .unwrap(),
            Some(0),
            "the mismatched firing enqueued nothing"
        );
    }

    #[pg_test]
    fn enable_happy_path() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b'), (NULL)").unwrap();

        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')").unwrap();
        assert!(id.is_some());

        // Shadow column added, correctly typed.
        let typ = Spi::get_one::<String>(
            "SELECT pg_catalog.format_type(atttypid, atttypmod) FROM pg_attribute
              WHERE attrelid = 'docs'::regclass AND attname = 'body_semantic'",
        )
        .unwrap();
        assert_eq!(typ.as_deref(), Some("vector(4)"));

        // Registry row present and active.
        let state = Spi::get_one::<String>(
            "SELECT state FROM postvec.registry WHERE source_column = 'body'",
        )
        .unwrap();
        assert_eq!(state.as_deref(), Some("active"));

        // Backfill enqueued the two non-null rows (NULL body skipped).
        let pending = Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap();
        assert_eq!(pending, Some(2));
    }

    /// A provider-backed model (external providers): the cache row carries
    /// the provider under raw->'extra' — exactly where discovery's
    /// `into_model_info` puts HubModel top-level extras. enable() resolves
    /// the dimension from the cache (no probe embed: there is no inference
    /// host in pg_test, so a probe would fail the call) and the NOTICE
    /// helper names the provider.
    #[pg_test]
    fn enable_on_a_provider_backed_model_uses_the_cached_dim() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('openai-text-embedding-3-small', 'embed', 'openai-text-embedding-3-small',
                     8,
                     '{\"name\": \"openai-text-embedding-3-small\",
                       \"extra\": {\"status\": \"provider\", \"provider\": \"openai\"}}'::jsonb)",
        )
        .unwrap();
        make_docs();

        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','openai-text-embedding-3-small')",
        )
        .unwrap();
        assert!(id.is_some());
        // Dimension came from the cached target_dim, not a live probe.
        let typ = Spi::get_one::<String>(
            "SELECT pg_catalog.format_type(atttypid, atttypmod) FROM pg_attribute
              WHERE attrelid = 'docs'::regclass AND attname = 'body_semantic'",
        )
        .unwrap();
        assert_eq!(typ.as_deref(), Some("vector(8)"));

        // The NOTICE predicate: provider-backed rows resolve their provider,
        // plain local models resolve none.
        assert_eq!(
            super::external_provider_of("openai-text-embedding-3-small").as_deref(),
            Some("openai")
        );
        seed_model("local-m", 4);
        assert_eq!(super::external_provider_of("local-m"), None);
    }

    #[pg_test]
    fn double_enable_refused() {
        seed_model("m", 4);
        make_docs();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')").ok();
        });
        assert!(r.is_err(), "second enable must error");
    }

    /// A dropped-and-recreated table leaves a stale active registry row (its
    /// triggers and shadow column died with the old table). enable() must
    /// detect the staleness, quarantine the old row, and succeed — the old
    /// behavior was a dead end ("already enabled" but disable() couldn't
    /// resolve the situation either).
    #[pg_test]
    fn reenable_after_table_recreation() {
        seed_model("m", 4);
        make_docs();
        let old_id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')")
            .unwrap()
            .unwrap();
        Spi::run("DROP TABLE docs").unwrap();
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('fresh')").unwrap();

        let new_id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')")
            .unwrap()
            .unwrap();
        assert_ne!(new_id, old_id, "a fresh registry entry");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.registry WHERE state = 'active'")
                .unwrap(),
            Some(1),
            "exactly one active entry; the stale one was cleared"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'"
            )
            .unwrap(),
            Some(4),
            "statement-mode triggers live on the new table"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(1),
            "backfill enqueued the new table's row"
        );
    }

    #[pg_test]
    fn no_pk_refused() {
        seed_model("m", 4);
        Spi::run("CREATE TABLE nopk (body text)").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.enable('nopk','body','m')").ok();
        });
        assert!(r.is_err(), "table without PK must be refused");
    }

    /// A temp table only exists in the creating session — the worker could
    /// never serve it, so enable() must refuse up front.
    /// Unlogged tables are allowed (with a durability warning).
    #[pg_test]
    fn temp_table_refused_unlogged_allowed() {
        seed_model("m", 4);
        Spi::run("CREATE TEMP TABLE tdocs (id bigint PRIMARY KEY, body text)").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.enable('tdocs','body','m')").ok();
        });
        assert!(r.is_err(), "temporary table must be refused");

        Spi::run("CREATE UNLOGGED TABLE udocs (id bigint PRIMARY KEY, body text)").unwrap();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('udocs','body','m')").unwrap();
        assert!(id.is_some(), "unlogged table is allowed (warned)");
    }

    /// Composite PKs are supported: the registry records the
    /// column/type arrays in index order and backfill keys rows as
    /// `ROW(...)::text`.
    #[pg_test]
    fn composite_pk_enable_works() {
        seed_model("m", 4);
        Spi::run("CREATE TABLE ck (b int, a int, body text, PRIMARY KEY (a, b))").unwrap();
        Spi::run("INSERT INTO ck VALUES (10, 1, 'x')").unwrap();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('ck','body','m')")
            .unwrap()
            .unwrap();

        let cols = Spi::get_one_with_args::<Vec<String>>(
            "SELECT pk_columns FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(cols, vec!["a", "b"], "index order, not attnum order");
        let types = Spi::get_one_with_args::<Vec<String>>(
            "SELECT pk_types FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(types, vec!["integer", "integer"]);

        // Backfill keyed the existing row by its record text.
        let pk = Spi::get_one::<String>("SELECT pk_value FROM postvec.jobs").unwrap();
        assert_eq!(pk.as_deref(), Some("(1,10)"));
    }

    /// `backfill_mode => 'cursor'` records the mode and enqueues nothing at
    /// enable time. The worker feeds chunks later.
    #[pg_test]
    fn cursor_backfill_enqueues_nothing_at_enable() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b')").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill_mode => 'cursor')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
        let mode = Spi::get_one_with_args::<String>(
            "SELECT backfill_mode FROM postvec.registry WHERE id = $1",
            &[id.into()],
        )
        .unwrap();
        assert_eq!(mode.as_deref(), Some("cursor"));
    }

    #[pg_test]
    fn unknown_model_refused() {
        make_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','ghost')").ok();
        });
        assert!(r.is_err(), "unknown model must be refused");
    }

    #[pg_test]
    fn disable_drops_triggers_and_optionally_column() {
        seed_model("m", 4);
        make_docs();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')").unwrap();

        Spi::run("SELECT postvec.disable('docs','body')").unwrap();
        let state = Spi::get_one::<String>(
            "SELECT state FROM postvec.registry WHERE source_column = 'body'",
        )
        .unwrap();
        assert_eq!(state.as_deref(), Some("disabled"));

        let ntrg = Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_trigger
              WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'",
        )
        .unwrap();
        assert_eq!(ntrg, Some(0), "triggers must be gone");

        // Column kept by default (count-based: get_one on zero rows errors).
        let col_exists = "SELECT count(*) FROM pg_attribute
                            WHERE attrelid = 'docs'::regclass
                              AND attname = 'body_semantic' AND NOT attisdropped";
        assert_eq!(
            Spi::get_one::<i64>(col_exists).unwrap(),
            Some(1),
            "column kept"
        );

        // drop_column removes it.
        Spi::run("SELECT postvec.disable('docs','body', drop_column => true)").unwrap();
        assert_eq!(
            Spi::get_one::<i64>(col_exists).unwrap(),
            Some(0),
            "column dropped"
        );
    }

    #[pg_test]
    fn distance_and_vector_index_use_the_right_opclass() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a')").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m',
                                   distance => 'l2', index_mode => 'immediate')",
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT distance FROM postvec.registry WHERE id = $1",
                &[id.into()]
            )
            .unwrap()
            .as_deref(),
            Some("l2")
        );
        // The HNSW index exists and uses the l2 opclass.
        let def = Spi::get_one::<String>(&format!(
            "SELECT pg_get_indexdef(('postvec_vec_{id}')::regclass)"
        ))
        .unwrap()
        .unwrap_or_default();
        assert!(def.contains("hnsw"), "index is HNSW: {def}");
        assert!(
            def.contains("vector_l2_ops"),
            "index uses l2 opclass: {def}"
        );
    }

    #[pg_test]
    fn invalid_distance_refused() {
        seed_model("m", 4);
        make_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.enable('docs','body','m', distance => 'manhattan')",
            )
            .ok();
        });
        assert!(r.is_err(), "unknown distance metric must be refused");
    }

    #[pg_test]
    fn create_vector_index_helper_is_idempotent() {
        seed_model("m", 4);
        make_docs();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')")
            .unwrap()
            .unwrap();
        // No vector index yet.
        assert!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_class WHERE relname = 'postvec_vec_{id}'"
            ))
            .unwrap()
            .unwrap()
                == 0
        );
        Spi::run("SELECT postvec.create_vector_index('docs','body')").unwrap();
        Spi::run("SELECT postvec.create_vector_index('docs','body')").unwrap(); // idempotent
        assert!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_class WHERE relname = 'postvec_vec_{id}'"
            ))
            .unwrap()
            .unwrap()
                == 1
        );
    }

    #[pg_test]
    fn row_trigger_mode_enqueues_per_change() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, note text)",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', trigger_mode => 'row', backfill => false)",
        )
        .unwrap();

        Spi::run("INSERT INTO docs (body, note) VALUES ('a','x'), ('b','y'), (NULL,'z')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(2),
            "row mode: two non-null rows enqueue, NULL skipped"
        );

        Spi::run("DELETE FROM postvec.jobs").unwrap();
        Spi::run("UPDATE docs SET note = 'changed'").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "row mode: AFTER UPDATE OF body does not fire for a note-only update"
        );

        Spi::run("UPDATE docs SET body = body").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "row mode: WHEN guard skips unchanged values"
        );
    }

    #[pg_test]
    fn statement_mode_pk_change_enqueues_new_key() {
        seed_model("m", 4);
        Spi::run("CREATE TABLE docs (id bigint PRIMARY KEY, body text)").unwrap();
        Spi::run("INSERT INTO docs (id, body) VALUES (1, 'a')").unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();

        Spi::run("UPDATE docs SET id = 11, body = 'changed' WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT pk_value FROM postvec.jobs")
                .unwrap()
                .as_deref(),
            Some("11"),
            "the PK-change companion trigger queues the new row identity"
        );
    }

    /// Row mode gets the same PK-change companion trigger as statement mode:
    /// an update that changes only the PK (not the source column) must re-key
    /// the row, or a pending job for the old key strands the change.
    #[pg_test]
    fn row_mode_pk_change_enqueues_new_key() {
        seed_model("m", 4);
        Spi::run("CREATE TABLE docs (id bigint PRIMARY KEY, body text)").unwrap();
        Spi::run("INSERT INTO docs (id, body) VALUES (1, 'a')").unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', trigger_mode => 'row', backfill => false)",
        )
        .unwrap();

        Spi::run("UPDATE docs SET id = 11 WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT pk_value FROM postvec.jobs")
                .unwrap()
                .as_deref(),
            Some("11"),
            "the PK-change companion trigger queues the new row identity in row mode"
        );
    }

    #[pg_test]
    fn generated_objects_depend_on_extension_when_possible() {
        seed_model("m", 4);
        make_docs();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
            .unwrap()
            .unwrap();
        let deps = Spi::get_one::<i64>(&format!(
            "SELECT count(*)
               FROM pg_depend d
               JOIN pg_extension e ON e.oid = d.refobjid
              WHERE e.extname = 'postvec'
                AND d.deptype = 'x'
                AND d.objid IN (
                    'postvec.trg_ins_{id}()'::regprocedure,
                    'postvec.trg_upd_{id}()'::regprocedure,
                    'postvec.trg_pk_{id}()'::regprocedure
                )"
        ))
        .unwrap();
        assert_eq!(deps, Some(3));
    }

    // ---- adopt() ----

    /// docs with a populated existing `embedding vector(4)` column: rows 1-2
    /// carry vectors, row 3 has text but no vector, row 4 has neither.
    fn make_adoptable_docs() {
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector(4))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO docs (body, embedding) VALUES
                ('a', '[1,2,3,4]'), ('b', '[5,6,7,8]'), ('c', NULL), (NULL, NULL)",
        )
        .unwrap();
    }

    fn postvec_trigger_count() -> i64 {
        Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_trigger
              WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'",
        )
        .unwrap()
        .unwrap()
    }

    #[pg_test]
    fn adopt_takes_over_a_populated_column() {
        seed_model("m", 4);
        make_adoptable_docs();
        let before =
            Spi::get_one::<String>("SELECT string_agg(embedding::text, '|' ORDER BY id) FROM docs")
                .unwrap();

        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap()
        .unwrap();

        // No ALTER TABLE happened: type and bytes are identical.
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass AND attname = 'embedding'"
            )
            .unwrap()
            .as_deref(),
            Some("vector(4)")
        );
        let after =
            Spi::get_one::<String>("SELECT string_agg(embedding::text, '|' ORDER BY id) FROM docs")
                .unwrap();
        assert_eq!(before, after, "stored vectors are byte-identical");

        assert_eq!(
            Spi::get_one_with_args::<bool>(
                "SELECT owns_vector_column FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap(),
            Some(false),
            "adopt() records that postvec does not own the column"
        );
        // Default backfill 'missing': only the NULL-vector row with text.
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(1)
        );
        assert_eq!(postvec_trigger_count(), 4, "synced: full trigger set");
    }

    /// The declared dimension is read from atttypmod even when the catalogue
    /// knows no dimension for the model (pins the atttypmod reading).
    #[pg_test]
    fn adopt_reads_the_declared_dim() {
        // Known as an embed model with NULL target_dim: catalogue is silent.
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, raw)
             VALUES ('m', 'embed', 'm', '{}'::jsonb)",
        )
        .unwrap();
        make_adoptable_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Spi::get_one_with_args::<i32>(
                "SELECT dim FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap(),
            Some(4),
            "the column's declared dimension is the authority"
        );
    }

    #[pg_test]
    fn adopt_refuses_dim_mismatch_and_inconsistent_catalogue() {
        seed_model("m8", 8);
        make_adoptable_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm8')",
            )
            .ok();
        });
        assert!(r.is_err(), "column vector(4) vs model dim 8 must refuse");

        // An internally inconsistent catalogue (two distinct dims for one
        // name across roles) must refuse even when one dim matches.
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, source_dim, target_model, raw)
             VALUES ('conv-mx-y', 'convert', 'mx', 4, 'y', '{}'::jsonb)",
        )
        .unwrap();
        seed_model("mx", 9);
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'mx')",
            )
            .ok();
        });
        assert!(r.is_err(), "dims {{4, 9}} for one name must refuse");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.registry").unwrap(),
            Some(0),
            "no registry row survives a refusal"
        );
    }

    #[pg_test]
    fn adopt_refuses_non_vector_column() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, arr real[], hv halfvec(4))",
        )
        .unwrap();
        for col in ["arr", "hv", "body"] {
            let q = format!(
                "SELECT postvec.adopt('docs','body', vector_column => '{col}', model => 'm')"
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "{col} is not pgvector's vector type");
        }
    }

    #[pg_test]
    fn adopt_refuses_unconstrained_vector() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector)",
        )
        .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "bare vector (no declared dim) must refuse");
    }

    #[pg_test]
    fn adopt_refuses_unwritable_vector_column() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector(4),
                                gen vector(4) GENERATED ALWAYS AS (embedding) STORED)",
        )
        .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'gen', model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "generated vector column must refuse");

        // A vector column inside the primary key (pgvector has btree ops).
        Spi::run("CREATE TABLE pkv (emb vector(4), body text, PRIMARY KEY (emb))").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('pkv','body', vector_column => 'emb', model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "PK-member vector column must refuse");
    }

    /// The complete NOT NULL matrix: only the exact no-worker-write
    /// configuration (sync => false AND backfill => 'none') is accepted, and
    /// every refusal leaves no registry row, trigger, or job behind.
    #[pg_test]
    fn adopt_not_null_requires_a_no_write_configuration() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector(4) NOT NULL)",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body, embedding) VALUES ('a', '[1,2,3,4]')").unwrap();

        for call in [
            "sync => true",
            "sync => false, backfill => 'missing'",
            "sync => false, backfill => 'all'",
        ] {
            let q = format!(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm', {call})"
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "NOT NULL must refuse: {call}");
            assert_eq!(
                Spi::get_one::<i64>("SELECT count(*) FROM postvec.registry").unwrap(),
                Some(0),
                "no registry row after refusal: {call}"
            );
            assert_eq!(
                postvec_trigger_count(),
                0,
                "no trigger after refusal: {call}"
            );
            assert_eq!(
                Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
                Some(0),
                "no job after refusal: {call}"
            );
        }

        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none')",
        )
        .unwrap();
        assert!(id.is_some(), "the exact no-write configuration is accepted");
    }

    /// Promotion rechecks NOT NULL before creating enqueue triggers or jobs
    /// (the write contract changes), then succeeds once NOT NULL is removed.
    #[pg_test]
    fn promotion_rechecks_not_null_before_writes() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector(4) NOT NULL)",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body, embedding) VALUES ('a', '[1,2,3,4]')").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(postvec_trigger_count(), 1, "sentinel only");

        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "promotion under NOT NULL must refuse");
        assert_eq!(postvec_trigger_count(), 1, "no enqueue trigger was added");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "no job was enqueued"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT trigger_mode FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("none"),
            "the entry stays observed"
        );

        Spi::run("ALTER TABLE docs ALTER COLUMN embedding DROP NOT NULL").unwrap();
        // A gap row inserted while observed (no triggers, nothing enqueued).
        Spi::run("INSERT INTO docs (body) VALUES ('gap')").unwrap();
        let id2 = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(id2, id, "promotion preserves the registry id");
        assert_eq!(
            postvec_trigger_count(),
            4,
            "full trigger set after promotion"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(1),
            "the missing-row backfill was enqueued"
        );
    }

    #[pg_test]
    fn adopt_refuses_missing_column() {
        seed_model("m", 4);
        make_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'ghost', model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "missing vector column must refuse");
    }

    #[pg_test]
    fn adopt_refuses_unknown_model() {
        make_adoptable_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'ghost')",
            )
            .ok();
        });
        assert!(r.is_err(), "a model unknown to postvec.models must refuse");
    }

    #[pg_test]
    fn adopt_refuses_column_claimed_by_another_entry() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, body2 text, embedding vector(4))",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body2', vector_column => 'embedding', \
                 model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "one active writer per vector column");
    }

    /// The database constraint behind the preflight: two active registry rows
    /// for one (schema, table, vector_column) violate the partial unique
    /// index even when both preflights raced past each other. (Last statement
    /// of the test: the unique violation aborts the test transaction.)
    #[pg_test]
    fn active_vector_column_unique_index_is_authoritative() {
        let ins = "INSERT INTO postvec.registry
                     (table_schema, table_name, source_column, vector_column,
                      pk_columns, pk_types, model, dim)
                   VALUES ('public','t',$1,'emb',ARRAY['id'],ARRAY['bigint'],'m',4)";
        Spi::run_with_args(ins, &["body1".into()]).unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run_with_args(ins, &["body2".into()]).ok();
        });
        assert!(
            r.is_err(),
            "the second active row for the same vector column must violate \
             registry_active_vector_target_key"
        );
    }

    #[pg_test]
    fn adopt_refuses_already_enabled_source_column() {
        seed_model("m", 4);
        make_adoptable_docs();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "an already-enabled source column must refuse");
    }

    #[pg_test]
    fn adopt_backfill_all_enqueues_every_row() {
        seed_model("m", 4);
        make_adoptable_docs();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  backfill => 'all')",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(3),
            "'all' enqueues every row with source text (the NULL-body row is skipped)"
        );
    }

    #[pg_test]
    fn adopt_backfill_none_enqueues_nothing() {
        seed_model("m", 4);
        make_adoptable_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  backfill => 'none')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT backfill_mode FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("none")
        );
    }

    #[pg_test]
    fn adopt_refuses_all_with_cursor() {
        seed_model("m", 4);
        make_adoptable_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm', backfill => 'all', backfill_mode => 'cursor')",
            )
            .ok();
        });
        assert!(r.is_err(), "'all' + 'cursor' must refuse");
    }

    /// Observed mode: only the exact TRUNCATE sentinel exists, DML
    /// enqueues nothing, and a dropped-and-recreated table is detected as a
    /// missing dependency.
    #[pg_test]
    fn adopt_sync_false_creates_only_truncate_sentinel() {
        seed_model("m", 4);
        make_adoptable_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(postvec_trigger_count(), 1, "exactly the TRUNCATE sentinel");
        assert_eq!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgrelid = 'docs'::regclass AND tgname = 'postvec_trunc_{id}'"
            ))
            .unwrap(),
            Some(1),
            "and it is the exact postvec_trunc_<id> name"
        );

        Spi::run("INSERT INTO docs (body) VALUES ('new row')").unwrap();
        Spi::run("UPDATE docs SET body = 'changed' WHERE id = 1").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "observed mode enqueues nothing on DML"
        );

        let entry = crate::registry::RegistryEntry::load(id).unwrap();
        assert_eq!(entry.trigger_mode, "none");
        assert!(
            !entry.triggers_missing(),
            "the sentinel satisfies the check"
        );
        assert!(entry.missing_dependency(&[]).is_none());

        // Drop and recreate the same-named table: the sentinel is gone, so
        // the entry must be reported as a dependency failure (quarantined by
        // whichever worker path sees it first).
        Spi::run("DROP TABLE docs").unwrap();
        make_adoptable_docs();
        assert!(entry.triggers_missing(), "sentinel gone after recreation");
        let reason = entry.missing_dependency(&[]);
        assert!(
            reason.as_deref().unwrap_or("").contains("sentinel"),
            "recreation is a dependency failure: {reason:?}"
        );
    }

    /// A model with no embed route (known only as a converter target whose
    /// source is not embeddable — the dead-model rescue case) adopts only in
    /// the observed no-backfill configuration.
    #[pg_test]
    fn adopt_observed_accepts_no_route_model_only_without_backfill() {
        // 'dead' is known as conv-x-dead's target (dim 4); source 'x' has no
        // embed model, so there is no embed route into 'dead'.
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('conv-x-dead', 'convert', 'x', 'dead', 4, '{}'::jsonb)",
        )
        .unwrap();
        make_adoptable_docs();

        for call in [
            "sync => true",
            "sync => false, backfill => 'missing'",
            "sync => false, backfill => 'all'",
        ] {
            let q = format!(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'dead', {call})"
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "no embed route must refuse: {call}");
        }
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'dead',
                                  sync => false, backfill => 'none')",
        )
        .unwrap();
        assert!(
            id.is_some(),
            "observed no-backfill adoption of a dead model works"
        );
    }

    /// Promotion: a second adopt() with sync => true attaches the
    /// enqueue triggers, keeps id and ownership, and enqueues the gap rows.
    #[pg_test]
    fn adopt_promotes_observed_entry_to_synced() {
        seed_model("m", 4);
        make_adoptable_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(postvec_trigger_count(), 1);

        let id2 = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(id2, id, "registry id preserved");
        assert_eq!(
            postvec_trigger_count(),
            4,
            "enqueue triggers added, sentinel kept"
        );
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT trigger_mode FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("statement")
        );
        assert_eq!(
            Spi::get_one_with_args::<bool>(
                "SELECT owns_vector_column FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap(),
            Some(false),
            "promotion does not change ownership"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(1),
            "the NULL-vector gap row was enqueued"
        );
        // A mismatched repeat call is still "already enabled".
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "an already-synced entry refuses re-adoption");
    }

    #[pg_test]
    fn if_not_exists_returns_the_matching_entry() {
        seed_model("m", 4);
        make_adoptable_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap();
        for repeat in [
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm', \
             if_not_exists => true)",
            "SELECT postvec.enable('docs','body','m', vector_column => 'embedding', \
             if_not_exists => true)",
        ] {
            assert_eq!(Spi::get_one::<i64>(repeat).unwrap(), id, "{repeat}");
        }
        for changed in [
            "SELECT postvec.enable('docs','body','other', vector_column => 'embedding', \
             if_not_exists => true)",
            "SELECT postvec.enable('docs','body','m', vector_column => 'embedding', \
             distance => 'l2', if_not_exists => true)",
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm', \
             format => '$body', if_not_exists => true)",
        ] {
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(changed).ok();
            });
            assert!(r.is_err(), "a changed option is refused: {changed}");
        }
    }

    #[pg_test(error = "text search configuration \"nope\" does not exist")]
    fn if_not_exists_reports_an_invalid_fts_config() {
        seed_model("m", 4);
        make_adoptable_docs();
        Spi::run("SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')")
            .unwrap();
        Spi::run(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm', \
             fts_config => 'nope', if_not_exists => true)",
        )
        .unwrap();
    }

    #[pg_test]
    fn disable_refuses_to_drop_an_adopted_column() {
        seed_model("m", 4);
        make_adoptable_docs();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap();

        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.disable('docs','body', drop_column => true)").ok();
        });
        assert!(r.is_err(), "drop_column on an adopted column must refuse");
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("active"),
            "the refused call left the entry alone"
        );

        Spi::run("SELECT postvec.disable('docs','body')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass
                    AND attname = 'embedding' AND NOT attisdropped"
            )
            .unwrap(),
            Some(1),
            "plain disable() keeps the adopted column"
        );
    }

    /// postvec-cli probes these EXACT signatures before acting
    /// (`postvec-cli/src/db/sql.rs`, `HAS_BUILD_INFO` / `HAS_UNINSTALL`);
    /// a mismatch makes `postvec uninstall` refuse with "upgrade postvec"
    /// against a current extension. Changing either signature requires
    /// updating the CLI probe in the same commit.
    #[pg_test]
    fn cli_probed_signatures_exist() {
        for probe in [
            "postvec.build_info()",
            "postvec.uninstall(boolean, boolean)",
        ] {
            assert_eq!(
                Spi::get_one_with_args::<bool>(
                    "SELECT to_regprocedure($1) IS NOT NULL",
                    &[probe.into()],
                )
                .unwrap(),
                Some(true),
                "the CLI-probed signature {probe} no longer exists — update \
                 postvec-cli/src/db/sql.rs together with the signature change"
            );
        }
    }

    /// uninstall(drop_columns => true) keeps adopted columns; a live
    /// migration's scratch `_new` column is dropped; a user column that
    /// reuses the `_new` name of a done/aborted migration survives.
    #[pg_test]
    fn uninstall_drop_columns_keeps_adopted_columns() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('m',  'embed',   NULL, 'm',  4, '{}'::jsonb),
                    ('m2', 'embed',   NULL, 'm2', 5, '{}'::jsonb),
                    ('conv-m-m2', 'convert', 'm', 'm2', 5, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        make_adoptable_docs();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap();
        // A live (running) migration owns its scratch column.
        Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").unwrap();

        // A second adopted entry whose *aborted* migration once used
        // 'embedding_new'; the user later created their own column with that
        // name — it must survive.
        Spi::run(
            "CREATE TABLE d2 (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                              body text, embedding vector(4))",
        )
        .unwrap();
        let id2 = Spi::get_one::<i64>(
            "SELECT postvec.adopt('d2','body', vector_column => 'embedding', model => 'm',
                                  backfill => 'none')",
        )
        .unwrap()
        .unwrap();
        Spi::run_with_args(
            "INSERT INTO postvec.migrations
                 (registry_id, old_model, new_model, old_dim, new_dim, strategy,
                  new_column, rows_total, state, finished_at)
             VALUES ($1, 'm', 'm2', 4, 5, 'convert', 'embedding_new', 0, 'aborted', now())",
            &[id2.into()],
        )
        .unwrap();
        Spi::run("ALTER TABLE d2 ADD COLUMN embedding_new vector(5)").unwrap();

        Spi::get_one::<i64>("SELECT postvec.uninstall(drop_columns => true)").unwrap();

        let col_exists = |tbl: &str, col: &str| -> bool {
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = to_regclass($1) AND attname = $2 AND NOT attisdropped",
                &[tbl.into(), col.into()],
            )
            .unwrap()
            .unwrap()
                > 0
        };
        assert!(col_exists("docs", "embedding"), "adopted column kept");
        assert!(
            !col_exists("docs", "embedding_new"),
            "the live migration's scratch column is cleaned"
        );
        assert!(col_exists("d2", "embedding"), "second adopted column kept");
        assert!(
            col_exists("d2", "embedding_new"),
            "a user column reusing an aborted migration's _new name survives"
        );
    }

    /// Teardown must never drop a same-named index it cannot prove it created
    /// (possible whenever postvec's own index was never built — e.g. an
    /// adopted entry): only indexes on the entry's table that carry the
    /// extension-dependency edge are postvec's to drop.
    #[pg_test]
    fn disable_leaves_same_named_user_index() {
        seed_model("m", 4);
        make_adoptable_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap()
        .unwrap();
        // A user index that happens to reuse postvec's conventional names,
        // on an unrelated table.
        Spi::run("CREATE TABLE other (x vector(4), body text)").unwrap();
        Spi::run(&format!(
            "CREATE INDEX postvec_vec_{id} ON other USING hnsw (x vector_cosine_ops)"
        ))
        .unwrap();
        Spi::run(&format!(
            "CREATE INDEX postvec_fts_{id} ON other USING gin (to_tsvector('english', body))"
        ))
        .unwrap();

        Spi::run("SELECT postvec.disable('docs','body')").unwrap();
        for idx in ["postvec_vec_", "postvec_fts_"] {
            assert!(
                Spi::get_one_with_args::<bool>(
                    "SELECT to_regclass($1) IS NOT NULL",
                    &[format!("{idx}{id}").into()],
                )
                .unwrap()
                .unwrap(),
                "{idx}{id} on the unrelated table survives teardown"
            );
        }
    }

    /// postvec's own generated indexes carry the extension dependency and are
    /// still dropped by disable().
    #[pg_test]
    fn disable_still_drops_postvec_created_indexes() {
        seed_model("m", 4);
        make_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false,
                                   create_fts_index => true, index_mode => 'immediate')",
        )
        .unwrap()
        .unwrap();
        for idx in ["postvec_vec_", "postvec_fts_"] {
            assert!(
                Spi::get_one_with_args::<bool>(
                    "SELECT to_regclass($1) IS NOT NULL",
                    &[format!("{idx}{id}").into()],
                )
                .unwrap()
                .unwrap(),
                "{idx}{id} exists after enable"
            );
        }
        Spi::run("SELECT postvec.disable('docs','body')").unwrap();
        for idx in ["postvec_vec_", "postvec_fts_"] {
            assert!(
                !Spi::get_one_with_args::<bool>(
                    "SELECT to_regclass($1) IS NOT NULL",
                    &[format!("{idx}{id}").into()],
                )
                .unwrap()
                .unwrap(),
                "{idx}{id} is dropped with the entry"
            );
        }
    }

    /// Index names are only schema-unique: creating postvec's conventional
    /// name must refuse when the name already belongs to something postvec
    /// cannot prove it created — and must never stamp the foreign object with
    /// the extension dependency (which would make it DROP EXTENSION
    /// collateral).
    #[pg_test]
    fn create_vector_index_never_claims_foreign_index() {
        seed_model("m", 4);
        make_docs();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
            .unwrap()
            .unwrap();
        Spi::run("CREATE TABLE other (x vector(4))").unwrap();
        Spi::run(&format!(
            "CREATE INDEX postvec_vec_{id} ON other USING hnsw (x vector_cosine_ops)"
        ))
        .unwrap();

        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.create_vector_index('docs','body')").ok();
        });
        assert!(
            r.is_err(),
            "a foreign same-named index is a collision, not a skip"
        );
        assert_eq!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_depend d
                   JOIN pg_extension e ON e.oid = d.refobjid
                  WHERE d.classid = 'pg_class'::regclass
                    AND d.objid = 'postvec_vec_{id}'::regclass
                    AND d.refclassid = 'pg_extension'::regclass
                    AND d.deptype = 'x' AND e.extname = 'postvec'"
            ))
            .unwrap(),
            Some(0),
            "the unrelated index never gains the extension dependency"
        );
    }

    /// Promotion must compare PK *types*, not just names: a PK column retyped
    /// since adoption (bigint -> text) would otherwise promote, and the
    /// worker would then cast queued keys through the stale recorded type and
    /// dead-letter them.
    #[pg_test]
    fn promotion_refuses_pk_type_drift() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, raw)
             VALUES ('m', 'embed', 'm', '{}'::jsonb)",
        )
        .unwrap();
        Spi::run("CREATE TABLE docs (id bigint PRIMARY KEY, body text, embedding vector(3))")
            .unwrap();
        Spi::run("INSERT INTO docs (id, body) VALUES (1, 'a')").unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none')",
        )
        .unwrap();

        Spi::run("ALTER TABLE docs ALTER COLUMN id TYPE text USING id::text").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "a retyped PK column must refuse promotion");
        assert_eq!(postvec_trigger_count(), 1, "no enqueue trigger was added");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "no job was enqueued"
        );
    }

    /// An invalid index (failed CREATE INDEX CONCURRENTLY leftover) is
    /// unusable by PostgreSQL and must not count as an ANN index anywhere:
    /// status(), the adopt-time advisory, or migration finalization.
    #[pg_test]
    fn invalid_index_is_not_usable() {
        seed_model("m", 4);
        make_docs();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
        Spi::run("CREATE INDEX docs_ann ON docs USING hnsw (body_semantic vector_cosine_ops)")
            .unwrap();
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'docs'::regclass::oid")
            .unwrap()
            .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT has_vector_index FROM postvec.status()").unwrap(),
            Some(true)
        );
        assert!(!super::ann_index_opclasses(oid, "body_semantic").is_empty());

        // Simulate the CONCURRENTLY failure leftover (superuser catalog DML).
        Spi::run("UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'docs_ann'::regclass")
            .unwrap();
        assert_eq!(
            Spi::get_one::<bool>("SELECT has_vector_index FROM postvec.status()").unwrap(),
            Some(false),
            "an invalid index does not satisfy the ANN probe"
        );
        assert!(
            super::ann_index_opclasses(oid, "body_semantic").is_empty(),
            "nor the opclass advisory"
        );
    }

    /// PRIMARY KEY ... INCLUDE (...) payload columns are not part of the row
    /// identity and must not be keyed on.
    #[pg_test]
    fn pk_include_columns_are_not_keyed() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE inc (id bigint, x int, body text,
                               PRIMARY KEY (id) INCLUDE (x))",
        )
        .unwrap();
        Spi::run("INSERT INTO inc VALUES (1, 2, 'a')").unwrap();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('inc','body','m')")
            .unwrap()
            .unwrap();
        assert_eq!(
            Spi::get_one_with_args::<Vec<String>>(
                "SELECT pk_columns FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .unwrap(),
            vec!["id"],
            "the INCLUDE column is not a key column"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT pk_value FROM postvec.jobs")
                .unwrap()
                .as_deref(),
            Some("1"),
            "rows key by the id alone, not a composite record"
        );
    }

    /// Promotion re-runs the whole adopted-column contract against the live
    /// catalog and refuses changes to the entry's immutable options — nothing
    /// is silently kept or silently accepted.
    #[pg_test]
    fn promotion_revalidates_column_and_refuses_option_changes() {
        // Model known with NULL dims so the catalogue stays silent and the
        // registry-vs-column dimension check is what fires.
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, raw)
             VALUES ('m', 'embed', 'm', '{}'::jsonb)",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                body text, embedding vector(3))",
        )
        .unwrap();
        Spi::run("INSERT INTO docs (body) VALUES ('a')").unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none')",
        )
        .unwrap()
        .unwrap();

        // The column was altered since adoption: promotion must refuse.
        Spi::run("ALTER TABLE docs ALTER COLUMN embedding TYPE vector(5)").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "dimension drift must refuse promotion");
        assert_eq!(postvec_trigger_count(), 1, "no enqueue trigger was added");
        Spi::run("ALTER TABLE docs ALTER COLUMN embedding TYPE vector(3)").unwrap();

        // A differing immutable option must refuse, not be silently retained.
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('docs','body', vector_column => 'embedding', \
                 model => 'm', distance => 'l2')",
            )
            .ok();
        });
        assert!(r.is_err(), "a changed distance must refuse promotion");
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT distance FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("cosine"),
            "the stored distance is untouched"
        );

        // The matching call still promotes.
        let id2 = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(id2, id);
        assert_eq!(postvec_trigger_count(), 4);
    }

    /// The opclass advisory probe sees expression ANN indexes (the halfvec
    /// form used above 2000 dims), not just direct key columns — and never a
    /// sibling column's index.
    #[pg_test]
    fn ann_index_opclasses_sees_expression_indexes() {
        Spi::run("CREATE TABLE t (id bigint PRIMARY KEY, v vector(4), v_new vector(4))").unwrap();
        Spi::run("CREATE INDEX t_expr ON t USING hnsw ((v::halfvec(4)) halfvec_l2_ops)").unwrap();
        Spi::run("CREATE INDEX t_new ON t USING hnsw (v_new vector_ip_ops)").unwrap();
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 't'::regclass::oid")
            .unwrap()
            .unwrap();
        let ops = super::ann_index_opclasses(oid, "v");
        assert_eq!(
            ops,
            vec!["halfvec_l2_ops".to_string()],
            "expression index found; sibling column's index excluded"
        );
    }

    /// search() on an observed entry whose sentinel is gone (table recreated)
    /// errors cleanly instead of querying the replacement table.
    #[pg_test]
    fn search_refuses_observed_entry_with_missing_sentinel() {
        seed_model("m", 4);
        make_adoptable_docs();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none')",
        )
        .unwrap();
        Spi::run("DROP TABLE docs").unwrap();
        make_adoptable_docs();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<String>(
                "SELECT pk_value FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0,0,0]::real[], 'q') LIMIT 1",
            )
            .ok();
        });
        assert!(
            r.is_err(),
            "the sentinel guard must refuse the recreated table"
        );
    }

    #[pg_test]
    fn uninstall_removes_generated_objects_and_optionally_columns() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a')").unwrap();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m')")
            .unwrap()
            .unwrap();

        let cleaned =
            Spi::get_one::<i64>("SELECT postvec.uninstall(drop_columns => true)").unwrap();
        assert_eq!(cleaned, Some(1));
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgrelid = 'docs'::regclass AND tgname LIKE 'postvec_%'"
            )
            .unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<i64>(&format!(
                "SELECT count(*) FROM pg_proc
                  WHERE proname IN ('trg_ins_{id}','trg_upd_{id}','trg_pk_{id}')"
            ))
            .unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM pg_attribute
                  WHERE attrelid = 'docs'::regclass
                    AND attname = 'body_semantic' AND NOT attisdropped"
            )
            .unwrap(),
            Some(0),
            "drop_columns removes the shadow column"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("disabled")
        );
    }

    // ---- Format templates ----

    fn make_ctx_docs() {
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                title text, body text, note text)",
        )
        .unwrap();
    }

    fn upd_funcdef(id: i64) -> String {
        Spi::get_one::<String>(&format!(
            "SELECT pg_get_functiondef('postvec.trg_upd_{id}()'::regprocedure)"
        ))
        .unwrap()
        .unwrap()
    }

    fn triggerdef(table: &str, name: &str) -> String {
        Spi::get_one_with_args::<String>(
            "SELECT pg_get_triggerdef(t.oid) FROM pg_trigger t
              WHERE t.tgrelid = to_regclass($1) AND t.tgname = $2",
            &[table.into(), name.into()],
        )
        .unwrap()
        .unwrap()
    }

    /// Pin the NULL-format trigger/function shape: the referenced-column
    /// generalisation must not drift the plain single-column DDL.
    #[pg_test]
    fn plain_entry_trigger_definitions_keep_their_shape() {
        seed_model("m", 4);
        make_ctx_docs();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
            .unwrap()
            .unwrap();
        let f = upd_funcdef(id);
        assert!(
            f.contains("WHERE n.\"body\" IS DISTINCT FROM o.\"body\""),
            "statement change predicate keeps the historical single-column shape: {f}"
        );
        let t = triggerdef("docs", &format!("postvec_upd_{id}"));
        assert!(
            t.contains("AFTER UPDATE ON") && !t.contains("UPDATE OF"),
            "statement trigger keeps no OF list (transition tables forbid it): {t}"
        );

        Spi::run(
            "CREATE TABLE rdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let rid = Spi::get_one::<i64>(
            "SELECT postvec.enable('rdocs','body','m', trigger_mode => 'row', backfill => false)",
        )
        .unwrap()
        .unwrap();
        let t = triggerdef("rdocs", &format!("postvec_upd_{rid}"));
        assert!(
            t.contains("AFTER UPDATE OF body ON"),
            "row trigger keeps the single-column OF list: {t}"
        );
        assert!(
            t.contains("new.body IS DISTINCT FROM old.body"),
            "row trigger keeps the single-column WHEN guard: {t}"
        );
    }

    /// Every template-validation refusal aborts the whole enable(): no
    /// registry row and no shadow column survive.
    #[pg_test]
    fn format_validation_refusals_leave_nothing() {
        seed_model("m", 4);
        make_ctx_docs();
        for (fmt, why) in [
            ("$body $ghost", "unknown referenced column"),
            ("$title", "template must reference the source column"),
            ("$body $body_semantic", "live vector column reference"),
            ("$body ${body_semantic_new}", "migration scratch reference"),
            ("", "empty template"),
            ("$body $", "dangling dollar"),
            ("$body ${x", "unclosed braced reference"),
        ] {
            let q = format!(
                "SELECT postvec.enable('docs','body','m', backfill => false, \
                 format => {})",
                crate::registry::quote_literal(fmt)
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "{why} must refuse");
            assert_eq!(
                Spi::get_one::<i64>("SELECT count(*) FROM postvec.registry").unwrap(),
                Some(0),
                "no registry row survives: {why}"
            );
            assert_eq!(
                Spi::get_one::<i64>(
                    "SELECT count(*) FROM pg_attribute
                      WHERE attrelid = 'docs'::regclass
                        AND attname = 'body_semantic' AND NOT attisdropped"
                )
                .unwrap(),
                Some(0),
                "no shadow column survives: {why}"
            );
        }
    }

    /// Statement and row triggers enqueue on referenced-column changes only;
    /// INSERT stays gated on the source column alone.
    #[pg_test]
    fn format_triggers_enqueue_on_referenced_changes_only() {
        seed_model("m", 4);
        make_ctx_docs();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false,
                                   format => '$title — $body')",
        )
        .unwrap();
        let pending = || -> i64 {
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs")
                .unwrap()
                .unwrap()
        };

        Spi::run("INSERT INTO docs (title, body, note) VALUES ('t','b','n')").unwrap();
        assert_eq!(pending(), 1, "insert with source text enqueues");
        // INSERT is gated on the source column, not the context columns.
        Spi::run("INSERT INTO docs (title, body) VALUES ('only title', NULL)").unwrap();
        assert_eq!(pending(), 1, "NULL source does not enqueue on INSERT");
        Spi::run("DELETE FROM postvec.jobs").unwrap();

        Spi::run("UPDATE docs SET note = 'changed' WHERE id = 1").unwrap();
        assert_eq!(pending(), 0, "unreferenced column change is ignored");
        Spi::run("UPDATE docs SET title = 'changed' WHERE id = 1").unwrap();
        assert_eq!(pending(), 1, "referenced context column change enqueues");
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        Spi::run("UPDATE docs SET title = title WHERE id = 1").unwrap();
        assert_eq!(pending(), 0, "IS DISTINCT FROM still filters no-op updates");

        // Row mode: same change set through OF-list + WHEN guard.
        Spi::run(
            "CREATE TABLE rdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                 title text, body text, note text)",
        )
        .unwrap();
        let rid = Spi::get_one::<i64>(
            "SELECT postvec.enable('rdocs','body','m', trigger_mode => 'row',
                                   backfill => false, format => '$title — $body')",
        )
        .unwrap()
        .unwrap();
        let t = triggerdef("rdocs", &format!("postvec_upd_{rid}"));
        assert!(
            t.contains("AFTER UPDATE OF title, body ON"),
            "row OF list covers the referenced set: {t}"
        );
        Spi::run("INSERT INTO rdocs (title, body, note) VALUES ('t','b','n')").unwrap();
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        Spi::run("UPDATE rdocs SET note = 'x'").unwrap();
        assert_eq!(pending(), 0);
        Spi::run("UPDATE rdocs SET title = 'x'").unwrap();
        assert_eq!(pending(), 1);
    }

    /// set_format() is an atomic full refresh: registry + trigger change set
    /// + every row enqueued (including NULL-source rows, whose refresh clears
    /// stale vectors); repeating the same value is a no-op; NULL clears.
    #[pg_test]
    fn set_format_replaces_triggers_and_refreshes_every_row() {
        seed_model("m", 3);
        make_ctx_docs();
        let id = Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
            .unwrap()
            .unwrap();
        Spi::run("INSERT INTO docs (title, body) VALUES ('t1', 'b1'), ('t2', NULL)").unwrap();
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        // Hand-place vectors: the NULL-source row carries a stale one.
        Spi::run("UPDATE docs SET body_semantic = '[1,1,1]'::vector").unwrap();

        Spi::run("SELECT postvec.set_format('docs','body', '$title — $body')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(2),
            "every row is enqueued, including the NULL-source one"
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT format FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("$title — $body")
        );
        let f = upd_funcdef(id);
        assert!(
            f.contains("n.\"title\" IS DISTINCT FROM o.\"title\""),
            "the replaced trigger covers the context column: {f}"
        );
        // The sentinel and the PK companion still exist (4 triggers total).
        assert_eq!(postvec_trigger_count(), 4);

        // Drain: the NULL-source row's refresh job clears its stale vector.
        drain_jobs_with_mock(3);
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs WHERE body IS NULL AND body_semantic IS NOT NULL"
            )
            .unwrap(),
            Some(0),
            "the NULL-source row's stale vector was cleared"
        );
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM docs WHERE body IS NOT NULL AND body_semantic IS NOT NULL"
            )
            .unwrap(),
            Some(1),
            "the live row was re-embedded"
        );

        // Unchanged value: a no-op — no jobs, no trigger churn.
        Spi::run("SELECT postvec.set_format('docs','body', '$title — $body')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "set_format with the identical template is a no-op"
        );

        // NULL clears the template and refreshes again.
        Spi::run("SELECT postvec.set_format('docs','body', NULL)").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(2)
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT format FROM postvec.registry").unwrap(),
            None
        );
        let f = upd_funcdef(id);
        assert!(
            !f.contains("title"),
            "the cleared template restores the single-column change set: {f}"
        );
    }

    /// Drain the queue through the mock pipeline (dim-`d` vectors).
    fn drain_jobs_with_mock(dim: usize) {
        use crate::jobs::{apply_group, claim_and_read, embed_with_bisection, split_items};
        let client = crate::jobs::mock::MockClient::new(dim);
        loop {
            let groups = claim_and_read(64, 300.0);
            if groups.is_empty() {
                return;
            }
            for g in groups {
                let routing = g.routing.clone();
                let (nj, ej) = split_items(g.items);
                let texts: Vec<String> = ej.iter().map(|(_, _, _, _, t)| t.clone()).collect();
                let outcomes =
                    embed_with_bisection(&client, "m", &Default::default(), 1000, &texts);
                apply_group(g.entry.id, &routing, &nj, &ej, &outcomes, 5, 5000);
            }
        }
    }

    /// set_format refuses: a migrating entry, an entry with no embed route,
    /// and a NOT NULL vector column (the refresh's NULL-source jobs would
    /// abort against it).
    #[pg_test]
    fn set_format_refuses_migrating_no_route_and_not_null() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('m',  'embed',   NULL, 'm',  4, '{}'::jsonb),
                    ('m2', 'embed',   NULL, 'm2', 5, '{}'::jsonb),
                    ('conv-m-m2', 'convert', 'm', 'm2', 5, '{}'::jsonb),
                    ('conv-x-dead', 'convert', 'x', 'dead', 4, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        make_ctx_docs();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
        Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.set_format('docs','body', '$title $body')").ok();
        });
        assert!(r.is_err(), "a migrating entry must refuse set_format");

        // Observed entry on a model with no embed route.
        Spi::run(
            "CREATE TABLE dead_docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                     title text, body text, embedding vector(4))",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('dead_docs','body', vector_column => 'embedding',
                                  model => 'dead', sync => false, backfill => 'none')",
        )
        .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.set_format('dead_docs','body', '$title $body')").ok();
        });
        assert!(r.is_err(), "no embed route must refuse set_format");

        // Observed entry with a NOT NULL vector column.
        Spi::run(
            "CREATE TABLE nn_docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                   title text, body text, embedding vector(4) NOT NULL)",
        )
        .unwrap();
        Spi::run("INSERT INTO nn_docs (body, embedding) VALUES ('b', '[1,2,3,4]')").unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.adopt('nn_docs','body', vector_column => 'embedding',
                                  model => 'm', sync => false, backfill => 'none')",
        )
        .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::run("SELECT postvec.set_format('nn_docs','body', '$title $body')").ok();
        });
        assert!(
            r.is_err(),
            "a NOT NULL vector column must refuse set_format"
        );
    }

    /// The explicit table lock is taken and held through commit — the exact
    /// full-refresh boundary even for observed entries (no triggers).
    #[pg_test]
    fn set_format_holds_the_table_lock_through_commit() {
        seed_model("m", 4);
        make_ctx_docs();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
        Spi::run("SELECT postvec.set_format('docs','body', 'x $body')").unwrap();
        let held = Spi::get_one::<i64>(
            "SELECT count(*) FROM pg_locks
              WHERE relation = 'docs'::regclass
                AND mode = 'ShareRowExclusiveLock' AND granted AND pid = pg_backend_pid()",
        )
        .unwrap();
        assert_eq!(held, Some(1), "SHARE ROW EXCLUSIVE held through commit");
    }

    /// A dropped referenced context column follows the quarantine path
    /// instead of crash-looping the worker on the generated SQL.
    #[pg_test]
    fn dropped_context_column_quarantines_cleanly() {
        seed_model("m", 4);
        make_ctx_docs();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('docs','body','m', backfill => false,
                                   format => '$title — $body')",
        )
        .unwrap()
        .unwrap();
        Spi::run("INSERT INTO docs (title, body) VALUES ('t','b')").unwrap();
        let entry = crate::registry::RegistryEntry::load(id).unwrap();

        Spi::run("ALTER TABLE docs DROP COLUMN title").unwrap();
        let reason = entry.missing_dependency(&[]);
        assert!(
            reason.as_deref().unwrap_or("").contains("title"),
            "the dropped context column is a dependency failure: {reason:?}"
        );
        let groups = crate::jobs::claim_and_read(64, 300.0);
        assert!(groups.is_empty(), "no work group for the broken entry");
        assert_eq!(
            Spi::get_one::<String>("SELECT state FROM postvec.registry")
                .unwrap()
                .as_deref(),
            Some("disabled"),
            "the entry was quarantined"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(0),
            "its jobs were purged"
        );
    }

    /// adopt(format => ..., backfill => 'all') stores the template and
    /// re-renders everything; promotion demands the bytewise-exact stored
    /// template ($body vs ${body} is a difference).
    #[pg_test]
    fn adopt_format_all_and_promotion_requires_exact_template() {
        seed_model("m", 4);
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                title text, body text, embedding vector(4))",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO docs (title, body, embedding) VALUES
                 ('t1', 'b1', '[1,2,3,4]'), ('t2', 'b2', NULL), (NULL, NULL, NULL)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.adopt('docs','body', vector_column => 'embedding', model => 'm',
                                  backfill => 'all', format => '$title $body')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            Spi::get_one_with_args::<String>(
                "SELECT format FROM postvec.registry WHERE id = $1",
                &[id.into()],
            )
            .unwrap()
            .as_deref(),
            Some("$title $body")
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap(),
            Some(2),
            "backfill 'all' re-renders every row with source text"
        );

        // Observed adoption with a template; promotion needs the exact bytes.
        Spi::run(
            "CREATE TABLE d2 (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                              title text, body text, embedding vector(4))",
        )
        .unwrap();
        let id2 = Spi::get_one::<i64>(
            "SELECT postvec.adopt('d2','body', vector_column => 'embedding', model => 'm',
                                  sync => false, backfill => 'none',
                                  format => '$title $body')",
        )
        .unwrap()
        .unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('d2','body', vector_column => 'embedding', model => 'm',
                                      format => '${title} $body')",
            )
            .ok();
        });
        assert!(
            r.is_err(),
            "a bytewise-different (even semantically equal) template refuses promotion"
        );
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT postvec.adopt('d2','body', vector_column => 'embedding', model => 'm')",
            )
            .ok();
        });
        assert!(r.is_err(), "omitting the stored template refuses promotion");
        let id2b = Spi::get_one::<i64>(
            "SELECT postvec.adopt('d2','body', vector_column => 'embedding', model => 'm',
                                  format => '$title $body')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(id2b, id2, "the exact stored template promotes in place");
    }

    // ---- retry_dead() ----

    /// Insert a dead-letter row directly (superuser test harness), returning
    /// its dead_id.
    fn dead_row(rid: i64, pk: &str) -> i64 {
        Spi::get_one_with_args::<i64>(
            "INSERT INTO postvec.jobs_dead
                 (job_id, registry_id, pk_value, attempts, last_error, created_at)
             VALUES (0, $1, $2, 6, 'seeded failure', now())
             RETURNING dead_id",
            &[rid.into(), pk.into()],
        )
        .unwrap()
        .unwrap()
    }

    fn dead_count() -> i64 {
        Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs_dead")
            .unwrap()
            .unwrap()
    }

    fn pending_count() -> i64 {
        Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs")
            .unwrap()
            .unwrap()
    }

    /// Re-drive all, then selected ids; fresh jobs start with natural
    /// attempts/error/claim state (nothing copied from the dead rows).
    #[pg_test]
    fn retry_dead_all_and_selected() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a'), ('b'), ('c')").unwrap();
        let rid =
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
                .unwrap()
                .unwrap();

        dead_row(rid, "1");
        dead_row(rid, "2");
        let moved = Spi::get_one::<i64>("SELECT postvec.retry_dead('docs','body')")
            .unwrap()
            .unwrap();
        assert_eq!(moved, 2, "the return value counts dead rows consumed");
        assert_eq!(dead_count(), 0);
        assert_eq!(pending_count(), 2);
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.jobs
                  WHERE attempts = 0 AND claimed_at IS NULL
                    AND last_error IS NULL AND not_before <= now()"
            )
            .unwrap(),
            Some(2),
            "re-driven jobs start with natural fresh state"
        );

        // Selected re-drive: only the named id moves.
        Spi::run("DELETE FROM postvec.jobs").unwrap();
        let d1 = dead_row(rid, "1");
        let _d2 = dead_row(rid, "3");
        let moved = Spi::get_one_with_args::<i64>(
            "SELECT postvec.retry_dead('docs','body', ARRAY[$1]::bigint[])",
            &[d1.into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(moved, 1);
        assert_eq!(dead_count(), 1, "the unselected dead row stays");
        assert_eq!(
            Spi::get_one::<String>("SELECT pk_value FROM postvec.jobs")
                .unwrap()
                .as_deref(),
            Some("1")
        );

        // An empty dead-letter queue re-drives zero rows without error.
        Spi::run("DELETE FROM postvec.jobs_dead").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT postvec.retry_dead('docs','body')")
                .unwrap()
                .unwrap(),
            0
        );
    }

    /// Unknown ids, another entry's ids, an empty array, and a NULL element
    /// all refuse without moving a single row.
    #[pg_test]
    fn retry_dead_input_refusals_move_nothing() {
        seed_model("m", 4);
        make_docs();
        let rid =
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
                .unwrap()
                .unwrap();
        Spi::run(
            "CREATE TABLE other (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let rid2 =
            Spi::get_one::<i64>("SELECT postvec.enable('other','body','m', backfill => false)")
                .unwrap()
                .unwrap();
        let mine = dead_row(rid, "1");
        let foreign = dead_row(rid2, "1");

        for (call, why) in [
            (
                format!("postvec.retry_dead('docs','body', ARRAY[{mine}, 999999]::bigint[])"),
                "unknown id",
            ),
            (
                format!("postvec.retry_dead('docs','body', ARRAY[{mine}, {foreign}]::bigint[])"),
                "another entry's id in a mixed list",
            ),
            (
                "postvec.retry_dead('docs','body', ARRAY[]::bigint[])".to_string(),
                "empty explicit array",
            ),
            (
                format!("postvec.retry_dead('docs','body', ARRAY[{mine}, NULL]::bigint[])"),
                "NULL element",
            ),
        ] {
            assert_refuses(&call, &format!("{why} must refuse"));
            assert_eq!(dead_count(), 2, "no dead row moved: {why}");
            assert_eq!(pending_count(), 0, "no job inserted: {why}");
        }
    }

    /// Duplicate dead PKs and an already-pending PK: every selected dead row
    /// is consumed (and counted), but the pending dedup index keeps at most
    /// one fresh job per row.
    #[pg_test]
    fn retry_dead_dedups_through_pending_index() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a')").unwrap();
        let rid =
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
                .unwrap()
                .unwrap();
        // Three dead rows sharing one PK, plus an already-pending job for it.
        dead_row(rid, "1");
        dead_row(rid, "1");
        dead_row(rid, "1");
        Spi::run_with_args(
            "INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ($1, '1')",
            &[rid.into()],
        )
        .unwrap();

        let moved = Spi::get_one::<i64>("SELECT postvec.retry_dead('docs','body')")
            .unwrap()
            .unwrap();
        assert_eq!(moved, 3, "all three dead rows are consumed and counted");
        assert_eq!(dead_count(), 0);
        assert_eq!(pending_count(), 1, "at most one pending job per row");
    }

    /// Assert a call errors, through a plpgsql exception block. For refusals
    /// raised inside the SECURITY DEFINER retry_dead(), catch_unwind would
    /// capture the error *without* unwinding PostgreSQL's security context
    /// stack (that cleanup runs at [sub]transaction abort), leaving the
    /// session unable to SET/RESET ROLE afterwards; the plpgsql EXCEPTION
    /// handler's subtransaction rollback restores it properly.
    fn assert_refuses(call: &str, why: &str) {
        let wrapped = format!(
            "DO $pv_t$ BEGIN
                PERFORM {call};
                RAISE EXCEPTION 'postvec-test: call unexpectedly succeeded';
            EXCEPTION WHEN others THEN
                IF SQLERRM LIKE '%unexpectedly succeeded%' THEN RAISE; END IF;
            END $pv_t$;"
        );
        Spi::run(&wrapped).unwrap_or_else(|e| panic!("{why}: {e}"));
    }

    /// Disabled and migrating entries refuse; so does a non-owner caller.
    #[pg_test]
    fn retry_dead_refuses_disabled_migrating_and_unauthorized() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, source_model, target_model, target_dim, raw)
             VALUES ('m',  'embed',   NULL, 'm',  4, '{}'::jsonb),
                    ('m2', 'embed',   NULL, 'm2', 5, '{}'::jsonb),
                    ('conv-m-m2', 'convert', 'm', 'm2', 5, '{}'::jsonb)
             ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        make_docs();
        let rid =
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
                .unwrap()
                .unwrap();
        dead_row(rid, "1");

        // Unauthorized: a plain role that does not own the table.
        Spi::run("CREATE ROLE pv_redrive_outsider").unwrap();
        Spi::run("GRANT SELECT ON docs TO pv_redrive_outsider").unwrap();
        Spi::run("SET ROLE pv_redrive_outsider").unwrap();
        assert_refuses(
            "postvec.retry_dead('docs','body')",
            "a non-owner must refuse",
        );
        Spi::run("RESET ROLE").unwrap();
        assert_eq!(dead_count(), 1, "nothing moved");

        // Migrating.
        Spi::get_one::<i64>("SELECT postvec.migrate('docs','body','m2')").unwrap();
        assert_refuses(
            "postvec.retry_dead('docs','body')",
            "a migrating entry must refuse",
        );
        let mid = Spi::get_one::<i64>("SELECT id FROM postvec.migrations")
            .unwrap()
            .unwrap();
        Spi::run(&format!("SELECT postvec.migration_abort({mid})")).unwrap();

        // Disabled.
        Spi::run("SELECT postvec.disable('docs','body')").unwrap();
        assert_refuses(
            "postvec.retry_dead('docs','body')",
            "a disabled entry must refuse",
        );
        assert_eq!(dead_count(), 1, "nothing ever moved");
    }

    /// The definer shape itself: prosecdef set, search_path pinned in
    /// proconfig, two owners each scoped to their own entry (including via
    /// legitimate SET ROLE), and an attacker-controlled search_path unable to
    /// redirect the control tables.
    #[pg_test]
    fn retry_dead_security_definer_is_scoped() {
        // Catalog shape: SECURITY DEFINER + pinned search_path.
        let (secdef, config) = (
            Spi::get_one::<bool>(
                "SELECT prosecdef FROM pg_proc
                  WHERE proname = 'retry_dead'
                    AND pronamespace = 'postvec'::regnamespace",
            )
            .unwrap(),
            Spi::get_one::<Vec<String>>(
                "SELECT proconfig FROM pg_proc
                  WHERE proname = 'retry_dead'
                    AND pronamespace = 'postvec'::regnamespace",
            )
            .unwrap()
            .unwrap_or_default(),
        );
        assert_eq!(secdef, Some(true), "retry_dead is SECURITY DEFINER");
        assert!(
            config
                .iter()
                .any(|c| c.starts_with("search_path=") && c.contains("pg_catalog")),
            "search_path is pinned in proconfig: {config:?}"
        );

        // Two owners, each restricted to their own entry.
        seed_model("m", 4);
        Spi::run("CREATE ROLE pv_owner_a").unwrap();
        Spi::run("CREATE ROLE pv_owner_b").unwrap();
        Spi::run("GRANT CREATE, USAGE ON SCHEMA public TO pv_owner_a, pv_owner_b").unwrap();
        Spi::run("CREATE TABLE ta (id bigint PRIMARY KEY, body text)").unwrap();
        Spi::run("CREATE TABLE tb (id bigint PRIMARY KEY, body text)").unwrap();
        Spi::run("ALTER TABLE ta OWNER TO pv_owner_a").unwrap();
        Spi::run("ALTER TABLE tb OWNER TO pv_owner_b").unwrap();
        let rid_a =
            Spi::get_one::<i64>("SELECT postvec.enable('ta','body','m', backfill => false)")
                .unwrap()
                .unwrap();
        let rid_b =
            Spi::get_one::<i64>("SELECT postvec.enable('tb','body','m', backfill => false)")
                .unwrap()
                .unwrap();
        let da = dead_row(rid_a, "1");
        let db = dead_row(rid_b, "1");

        // Owner A via legitimate SET ROLE: may move their own rows...
        Spi::run("SET ROLE pv_owner_a").unwrap();
        let moved = Spi::get_one_with_args::<i64>(
            "SELECT postvec.retry_dead('ta','body', ARRAY[$1]::bigint[])",
            &[da.into()],
        )
        .unwrap()
        .unwrap();
        assert_eq!(moved, 1, "an owner re-drives their own entry via SET ROLE");
        // ...but not B's — neither through B's table (not the owner) nor by
        // smuggling B's dead id into their own entry's list.
        assert_refuses(
            "postvec.retry_dead('tb','body')",
            "A cannot re-drive B's table",
        );
        assert_refuses(
            &format!("postvec.retry_dead('ta','body', ARRAY[{db}]::bigint[])"),
            "B's dead id in A's list must refuse",
        );
        Spi::run("RESET ROLE").unwrap();
        assert_eq!(dead_count(), 1, "B's dead row never moved");

        // Attacker-controlled search_path: decoy control tables must never be
        // touched — the definer's pinned path plus qualified references keep
        // every object reference on the real postvec schema.
        Spi::run("CREATE SCHEMA pv_attack").unwrap();
        Spi::run(
            "CREATE TABLE pv_attack.jobs_dead (dead_id bigint, registry_id bigint,
                                               pk_value text)",
        )
        .unwrap();
        Spi::run("CREATE TABLE pv_attack.jobs (registry_id bigint, pk_value text)").unwrap();
        Spi::run_with_args(
            "INSERT INTO pv_attack.jobs_dead VALUES (424242, $1, 'decoy')",
            &[rid_b.into()],
        )
        .unwrap();
        Spi::run("SET search_path TO pv_attack, public, postvec").unwrap();
        Spi::run("SET ROLE pv_owner_b").unwrap();
        let moved = Spi::get_one::<i64>("SELECT postvec.retry_dead('public.tb','body')")
            .unwrap()
            .unwrap();
        Spi::run("RESET ROLE").unwrap();
        Spi::run("RESET search_path").unwrap();
        assert_eq!(moved, 1, "the real dead row moved");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM pv_attack.jobs_dead").unwrap(),
            Some(1),
            "the decoy table is untouched"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM pv_attack.jobs").unwrap(),
            Some(0),
            "no job leaked into the decoy queue"
        );
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.jobs WHERE registry_id = $1",
                &[rid_b.into()],
            )
            .unwrap(),
            Some(1),
            "the fresh job landed in the real queue"
        );
    }

    /// Sequential half of the concurrency contract: once a call consumed a
    /// set of explicit ids, a second call for the same ids reports that they
    /// no longer exist instead of pretending to move them. (The two-session
    /// half — the second caller *waiting* on the first's FOR UPDATE locks —
    /// is exactly what the post-lock revalidation this test exercises is
    /// for; the pg_test harness runs one session, so the blocking itself is
    /// PostgreSQL's row-lock semantics.)
    #[pg_test]
    fn retry_dead_consumed_ids_report_gone() {
        seed_model("m", 4);
        make_docs();
        Spi::run("INSERT INTO docs (body) VALUES ('a')").unwrap();
        let rid =
            Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)")
                .unwrap()
                .unwrap();
        let d = dead_row(rid, "1");
        assert_eq!(
            Spi::get_one_with_args::<i64>(
                "SELECT postvec.retry_dead('docs','body', ARRAY[$1]::bigint[])",
                &[d.into()],
            )
            .unwrap(),
            Some(1)
        );
        assert_refuses(
            &format!("postvec.retry_dead('docs','body', ARRAY[{d}]::bigint[])"),
            "the second call must report the ids as gone",
        );
        assert_eq!(pending_count(), 1, "exactly one job from the first call");
    }
}
