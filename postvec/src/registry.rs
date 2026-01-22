//! Shared registry data model + SQL helpers used by the worker, search, and
//! status. The `enable()`/`disable()` SQL functions live in
//! [`crate::api::registry`]; this module is the transport-agnostic core those
//! and everything else read through.

use pgrx::prelude::*;

/// One enabled (table, column) as stored in `postvec.registry`.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub id: i64,
    pub table_schema: String,
    pub table_name: String,
    pub source_column: String,
    pub vector_column: String,
    /// PK columns in index order; length 1 for a plain PK, >1 for composite.
    pub pk_columns: Vec<String>,
    /// `format_type` of each PK column; cast targets for watermark literals.
    pub pk_types: Vec<String>,
    pub model: String,
    pub dim: i32,
    /// FTS text-search configuration name (regconfig rendered as text).
    pub fts_config: String,
    /// Distance metric: `cosine` | `l2` | `ip`. Drives the pgvector operator
    /// used by `search()` and the opclass used when building the index.
    pub distance: String,
    /// `none` | `queue` | `cursor` | `done`. Cursor is the large-table path.
    pub backfill_mode: String,
    pub backfill_watermark: Option<String>,
    pub state: String,
    /// `statement` | `row` | `none` — `none` is an adopted observed entry
    /// (no DML enqueue triggers; only the TRUNCATE identity sentinel).
    pub trigger_mode: String,
    /// Whether teardown may drop `vector_column`: true for enable()-created
    /// (and migration-finalized) columns, false for adopted ones.
    pub owns_vector_column: bool,
    /// Document-embedding template, exactly as validated at
    /// enable/adopt/set_format time. `None` embeds the raw source column.
    pub format: Option<String>,
    /// `manual` | `immediate` | `auto`. Only `auto` reconciles later.
    pub index_mode: String,
    /// The parked automatic-build failure, if any.
    pub index_error: Option<String>,
    /// `none` (one row, one vector on the source table) or `recursive`
    /// (one row, many chunk vectors in the managed destination).
    pub chunking: String,
    /// Splitter geometry, non-NULL exactly for recursive entries.
    pub chunk_size: Option<i32>,
    pub chunk_overlap: Option<i32>,
    /// Managed destination table/view, non-NULL exactly for recursive
    /// entries. The view is `<destination_table>_view`.
    pub destination_schema: Option<String>,
    pub destination_table: Option<String>,
    pub destination_view: Option<String>,
    /// Destination ownership token, mirrored in exact comments on the table
    /// and view. Destructive teardown requires the match.
    pub destination_token: Option<String>,
}

impl RegistryEntry {
    /// `"schema"."table"` fully-quoted for interpolation into dynamic SQL.
    ///
    /// This is the **source** relation. Kept under its historical name as a
    /// compatibility alias while callers migrate; code that creates,
    /// searches, indexes, or migrates *vectors* must go through
    /// [`RegistryEntry::qualified_vector_table`] deliberately.
    pub fn qualified_table(&self) -> String {
        format!(
            "{}.{}",
            quote_ident(&self.table_schema),
            quote_ident(&self.table_name)
        )
    }

    /// Whether this entry is in recursive chunking mode.
    pub fn is_recursive(&self) -> bool {
        self.chunking == "recursive"
    }

    /// The source relation, fully quoted (explicit spelling of
    /// [`RegistryEntry::qualified_table`]).
    pub fn qualified_source_table(&self) -> String {
        self.qualified_table()
    }

    /// The relation that physically holds this entry's vectors: the source
    /// table in column mode, the managed destination in recursive mode.
    pub fn qualified_vector_table(&self) -> String {
        match (&self.destination_schema, &self.destination_table) {
            (Some(schema), Some(table)) => {
                format!("{}.{}", quote_ident(schema), quote_ident(table))
            }
            _ => self.qualified_table(),
        }
    }

    /// The generated join view for a recursive entry, fully quoted.
    pub fn qualified_destination_view(&self) -> Option<String> {
        match (&self.destination_schema, &self.destination_view) {
            (Some(schema), Some(view)) => {
                Some(format!("{}.{}", quote_ident(schema), quote_ident(view)))
            }
            _ => None,
        }
    }

    /// Native-typed join between the source row and its destination chunks:
    /// `<source_alias>.<pk> = <dest_alias>.postvec_source_pk`. Both sides
    /// carry the source PK's exact type, so both PK indexes stay usable and
    /// there is no cast. Recursive entries have a single-column PK.
    pub fn source_pk_join(&self, source_alias: &str, dest_alias: &str) -> String {
        format!(
            "{}.{} = {dest_alias}.postvec_source_pk",
            source_alias,
            quote_ident(&self.pk_columns[0]),
        )
    }

    pub fn is_composite_pk(&self) -> bool {
        self.pk_columns.len() > 1
    }

    /// The text key expression jobs are keyed by, optionally prefixed with a
    /// table alias: `"id"::text` for a plain PK, `ROW("a","b")::text` for a
    /// composite one (record text output is unambiguous and unique per key).
    pub fn pk_text_expr(&self, alias: &str) -> String {
        let prefix = if alias.is_empty() {
            String::new()
        } else {
            format!("{alias}.")
        };
        if self.is_composite_pk() {
            let cols: Vec<String> = self
                .pk_columns
                .iter()
                .map(|c| format!("{prefix}{}", quote_ident(c)))
                .collect();
            format!("ROW({})::text", cols.join(", "))
        } else {
            format!("{prefix}{}::text", quote_ident(&self.pk_columns[0]))
        }
    }

    fn alias_prefix(alias: &str) -> String {
        if alias.is_empty() {
            String::new()
        } else {
            format!("{alias}.")
        }
    }

    /// Predicate for looking up queued text keys against the source table.
    /// For the common single-column PK path, cast the parameter side back to
    /// the native PK type so the primary-key index remains usable. Composite
    /// keys keep the record-text representation.
    pub fn pk_any_clause(&self, alias: &str, param: &str) -> String {
        if self.is_composite_pk() {
            format!("{} = ANY({param})", self.pk_text_expr(alias))
        } else {
            format!(
                "{}{} = ANY({param}::{}[])",
                Self::alias_prefix(alias),
                quote_ident(&self.pk_columns[0]),
                self.pk_types[0],
            )
        }
    }

    /// Predicate joining a source table row to a text-key staging relation.
    /// Single-column PKs again cast the staging value, not the indexed column.
    pub fn pk_staging_join_clause(
        &self,
        alias: &str,
        staging_alias: &str,
        staging_col: &str,
    ) -> String {
        if self.is_composite_pk() {
            format!(
                "{} = {staging_alias}.{}",
                self.pk_text_expr(alias),
                quote_ident(staging_col)
            )
        } else {
            format!(
                "{}{} = {staging_alias}.{}::{}",
                Self::alias_prefix(alias),
                quote_ident(&self.pk_columns[0]),
                quote_ident(staging_col),
                self.pk_types[0],
            )
        }
    }

    /// Deterministic total-order expression for watermark iteration (cursor
    /// backfill, migration driver). Plain PKs order natively (index-friendly);
    /// composite PKs order by their record text under the stable "C" collation
    /// (documented as a sequential-scan path).
    pub fn pk_order_expr(&self, alias: &str) -> String {
        if self.is_composite_pk() {
            format!("{} COLLATE \"C\"", self.pk_text_expr(alias))
        } else {
            let prefix = if alias.is_empty() {
                String::new()
            } else {
                format!("{alias}.")
            };
            format!("{prefix}{}", quote_ident(&self.pk_columns[0]))
        }
    }

    /// `AND <order expr> > <literal>` clause resuming after a stored watermark
    /// (`pk_text_expr` output for composite, `pk::text` for plain — both cast
    /// back losslessly).
    pub fn pk_watermark_clause(&self, alias: &str, watermark: &str) -> String {
        if self.is_composite_pk() {
            format!(
                " AND {} > {} COLLATE \"C\"",
                self.pk_order_expr(alias),
                quote_literal(watermark)
            )
        } else {
            format!(
                " AND {} > {}::{}",
                self.pk_order_expr(alias),
                quote_literal(watermark),
                self.pk_types[0]
            )
        }
    }

    /// Check the entry's physical dependencies still exist: the relation, its
    /// source and vector columns, plus any `extra_columns` (e.g. a migration's
    /// new column). Returns a human-readable reason when something is gone —
    /// the worker uses this to quarantine broken entries instead of
    /// crash-looping on SPI errors (DROP TABLE / DROP COLUMN while enabled).
    /// Must be called inside a transaction. This catalog-only form is for
    /// reconciliation and error-reporting paths that do not subsequently run
    /// generated SQL. Worker read/render paths must use
    /// [`RegistryEntry::missing_dependency_locked`] so the proved definitions
    /// cannot change before that SQL executes.
    pub fn missing_dependency(&self, extra_columns: &[&str]) -> Option<String> {
        self.missing_dependency_inner(extra_columns, false)
    }

    /// Validate the same dependency contract while pinning the source and,
    /// for recursive entries, destination definitions with ACCESS SHARE until
    /// transaction end. Call this before generated worker SQL, and before
    /// taking registry/job row locks, to preserve the global
    /// source -> destination -> registry -> jobs lock order.
    pub fn missing_dependency_locked(&self, extra_columns: &[&str]) -> Option<String> {
        self.missing_dependency_inner(extra_columns, true)
    }

    fn missing_dependency_inner(
        &self,
        extra_columns: &[&str],
        lock_relations: bool,
    ) -> Option<String> {
        // A hand-edited registry row with no PK columns would otherwise panic
        // every path that indexes pk_columns[0]/pk_types[0]; treat it as a
        // quarantinable defect like any other broken dependency.
        if self.pk_columns.is_empty() || self.pk_types.is_empty() {
            return Some(
                "its registry row has no primary-key columns (hand-edited postvec.registry?)"
                    .to_string(),
            );
        }
        let qualified = self.qualified_table();
        let rel_oid = Spi::get_one_with_args::<pg_sys::Oid>(
            "SELECT to_regclass($1)::oid",
            &[qualified.as_str().into()],
        )
        .unwrap_or(None);
        let Some(rel_oid) = rel_oid else {
            return Some(format!("relation {qualified} no longer exists"));
        };
        if lock_relations {
            // Hold the relation's definition still for the rest of this
            // transaction, including generated measure/render SQL. ACCESS
            // SHARE conflicts only with ACCESS EXCLUSIVE DDL. Callers must
            // take this before registry/job row locks.
            unsafe {
                pg_sys::LockRelationOid(rel_oid, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
            }
            // Re-prove the name still maps to the locked OID: a concurrent
            // drop-and-recreate between resolution and lock would otherwise
            // leave us validating a relation we never locked.
            let still_same = Spi::get_one_with_args::<bool>(
                "SELECT to_regclass($1)::oid = $2",
                &[qualified.as_str().into(), rel_oid.into()],
            )
            .unwrap_or(Some(false))
            .unwrap_or(false);
            if !still_same {
                return Some(format!(
                    "relation {qualified} was dropped or replaced during validation"
                ));
            }
        }
        // In recursive mode the vector column and every scratch column live
        // on the destination, not the source. Probing them against the
        // source would quarantine every healthy chunked entry.
        let mut columns: Vec<&str> = if self.is_recursive() {
            vec![&self.source_column]
        } else {
            let mut cols = vec![self.source_column.as_str(), self.vector_column.as_str()];
            cols.extend_from_slice(extra_columns);
            cols
        };
        // The current template's referenced context columns are physical
        // dependencies of the generated worker SQL. A dropped one must
        // quarantine the entry instead of crash-looping the worker on the
        // same generated statement. For a recursive entry the `chunk`
        // pseudo-column is already excluded by referenced_columns().
        let referenced = match self.referenced_columns() {
            Ok(refs) => refs,
            Err(e) => return Some(format!("its stored format template is invalid: {e}")),
        };
        for col in &referenced {
            if !columns.contains(&col.as_str()) {
                columns.push(col);
            }
        }
        for col in columns {
            let exists = Spi::get_one_with_args::<bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_attribute
                      WHERE attrelid = $1 AND attname = $2
                        AND attnum > 0 AND NOT attisdropped)",
                &[rel_oid.into(), col.into()],
            )
            .unwrap_or(Some(false))
            .unwrap_or(false);
            if !exists {
                return Some(format!("column {col:?} no longer exists on {qualified}"));
            }
            // enable()/adopt()/set_format() classify rendered columns by
            // base-type OID, but ALTER TABLE ... ALTER COLUMN TYPE after
            // enable can turn an admitted column into one whose `::text`
            // rendering the byte ceilings cannot measure without
            // materializing it (jsonb/bytea/arrays/composites). The worker
            // repeats the classification on every dependency check and
            // quarantines instead of running generated SQL whose ceilings
            // are no longer real bounds. Rendered columns are the source
            // column and the template's context columns, never the vector
            // or scratch columns (those types are pgvector's business).
            let rendered = col == self.source_column || referenced.iter().any(|r| r == col);
            if rendered && !crate::api::registry::boundable_column(rel_oid, col) {
                return Some(format!(
                    "column {col:?} on {qualified} no longer has a byte-boundable type \
                     (ALTER COLUMN TYPE after enable?)"
                ));
            }
        }
        if self.is_recursive() {
            if let Some(reason) =
                self.missing_destination_dependency_inner(extra_columns, lock_relations)
            {
                return Some(reason);
            }
        }
        // An observed entry (trigger_mode = 'none') has no DML triggers, so a
        // dropped-and-recreated same-named table would otherwise re-resolve
        // by name and silently attach the old entry to a different relation.
        // Its exact TRUNCATE sentinel is the relation-identity check: when
        // it is gone, the entry is a dependency failure so write-back,
        // cursor backfill and the migration driver quarantine instead of
        // touching the replacement table. Synced hot paths pay no extra
        // catalog query.
        if self.trigger_mode == "none" && self.truncate_sentinel_missing() {
            return Some(format!(
                "its observed-entry TRUNCATE sentinel is gone from {qualified} \
                 (table recreated?)"
            ));
        }
        None
    }

    /// The exact ownership-marker comments `enable()` writes on the
    /// destination table and view, reproducible from registry state alone
    /// (they must survive dump/restore byte-identically — comments do; OIDs
    /// do not).
    pub fn destination_comments(&self) -> Option<(String, String)> {
        let token = self.destination_token.as_deref()?;
        let table = format!(
            "postvec: managed chunk destination for {}.{}.{} (registry entry {}); ownership \
             token {token}. Automatic teardown requires this exact comment.",
            self.table_schema, self.table_name, self.source_column, self.id,
        );
        let view = format!(
            "postvec: managed chunk join view for {}.{}.{} (registry entry {}); ownership \
             token {token}. Automatic teardown requires this exact comment.",
            self.table_schema, self.table_name, self.source_column, self.id,
        );
        Some((table, view))
    }

    /// Destination-side identity contract of a recursive entry. The same
    /// validator sits behind worker reconciliation
    /// ([`RegistryEntry::missing_dependency`]) and destructive teardown.
    /// It proves, in order:
    ///
    /// 1. the destination is an ordinary table whose comment is the exact
    ///    generated ownership marker
    /// 2. the fixed chunk columns exist with their exact types and the
    ///    identity property on `postvec_chunk_id`
    /// 3. the vector column is pgvector's exact `vector` type at the
    ///    entry's exact dimension (an altered dimension must quarantine,
    ///    not make write-backs fail forever)
    /// 4. `postvec_source_pk` matches the live source PK attribute's type
    ///    OID, typmod and collation (skipped when the source relation is
    ///    gone: teardown after `DROP ... CASCADE` still proves the rest)
    /// 5. the `UNIQUE (postvec_source_pk, postvec_chunk_seq)` key exists
    ///    (invalidation SQL and refresh replace depend on it)
    /// 6. if the view still exists it is a view carrying the exact marker
    ///    and depending on the destination (and the source, while that
    ///    exists); while the source exists the view must exist
    /// 7. any extra (migration scratch) columns exist
    ///
    /// A failed check is a quarantine/refusal reason, never a worker crash
    /// loop. A same-named object that fails it must never be written,
    /// indexed or dropped.
    pub fn missing_destination_dependency(&self, extra_columns: &[&str]) -> Option<String> {
        self.missing_destination_dependency_inner(extra_columns, false)
    }

    fn missing_destination_dependency_inner(
        &self,
        extra_columns: &[&str],
        lock_relation: bool,
    ) -> Option<String> {
        let (Some(schema), Some(table), Some(view)) = (
            self.destination_schema.as_deref(),
            self.destination_table.as_deref(),
            self.destination_view.as_deref(),
        ) else {
            return Some("its recursive registry row lacks destination fields".into());
        };
        let Some((expected_table_comment, expected_view_comment)) = self.destination_comments()
        else {
            return Some("its recursive registry row lacks the ownership token".into());
        };
        let qdest = format!("{}.{}", quote_ident(schema), quote_ident(table));
        let qview = format!("{}.{}", quote_ident(schema), quote_ident(view));

        // Resolve the destination once for the catalog proof. Locked worker
        // validation additionally pins it after the source and before any
        // registry/job row lock; catalog-only reconciliation deliberately
        // does not retain relation locks.
        let dest_oid = Spi::get_one_with_args::<pg_sys::Oid>(
            "SELECT to_regclass($1)::oid",
            &[qdest.as_str().into()],
        )
        .unwrap_or(None);
        let Some(dest_oid) = dest_oid else {
            return Some(format!("its destination table {qdest} no longer exists"));
        };
        if lock_relation {
            unsafe {
                pg_sys::LockRelationOid(dest_oid, pg_sys::AccessShareLock as pg_sys::LOCKMODE);
            }
            let still_same = Spi::get_one_with_args::<bool>(
                "SELECT to_regclass($1)::oid = $2",
                &[qdest.as_str().into(), dest_oid.into()],
            )
            .unwrap_or(Some(false))
            .unwrap_or(false);
            if !still_same {
                return Some(format!(
                    "its destination {qdest} was dropped or replaced during validation"
                ));
            }
        }

        // (1) relation kind, exact marker, and the row-security posture the
        // visibility contract depends on: application roles may hold direct
        // SELECT on the destination, so silently losing ENABLE/FORCE RLS or
        // the source-visibility policy would widen what they can read.
        // Locked worker callers read these facts after the relation lock makes
        // them stable through their generated SQL.
        let facts: Option<(String, Option<String>, bool)> = Spi::connect(|c| {
            let t = c
                .select(
                    "SELECT c.relkind::text, obj_description(c.oid, 'pg_class'),
                            c.relrowsecurity AND c.relforcerowsecurity
                       FROM pg_class c WHERE c.oid = $1",
                    Some(1),
                    &[dest_oid.into()],
                )
                .ok()?;
            t.into_iter().next().map(|r| {
                (
                    r.get::<String>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap(),
                    r.get::<bool>(3).unwrap().unwrap_or(false),
                )
            })
        });
        let Some((relkind, comment, rls_ok)) = facts else {
            return Some(format!("its destination table {qdest} no longer exists"));
        };
        if relkind != "r" {
            return Some(format!(
                "its destination {qdest} is not an ordinary table (relkind={relkind})"
            ));
        }
        if !rls_ok {
            return Some(format!(
                "its destination {qdest} no longer has row security enabled AND forced — \
                 the source-visibility contract for direct reads is broken"
            ));
        }
        if comment.as_deref() != Some(expected_table_comment.as_str()) {
            return Some(format!(
                "its destination {qdest} does not carry the entry's exact ownership marker \
                 (recreated under the same name, or the comment was edited?); postvec will \
                 not write to it or drop it — inspect it and, if it is yours to remove, run \
                 DROP TABLE {qdest}; yourself"
            ));
        }

        // (2) fixed columns with exact types + the identity property.
        let fixed_ok = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pg_attribute a
              WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
                AND (
                    (a.attname = 'postvec_chunk_id'
                         AND a.atttypid = 'pg_catalog.int8'::regtype
                         AND a.attidentity <> '')
                 OR (a.attname = 'postvec_chunk_seq'
                         AND a.atttypid = 'pg_catalog.int4'::regtype)
                 OR (a.attname = 'postvec_char_start'
                         AND a.atttypid = 'pg_catalog.int8'::regtype)
                 OR (a.attname = 'postvec_char_end'
                         AND a.atttypid = 'pg_catalog.int8'::regtype)
                 OR (a.attname = 'chunk_text'
                         AND a.atttypid = 'pg_catalog.text'::regtype)
                )",
            &[dest_oid.into()],
        )
        .unwrap_or(Some(0))
        .unwrap_or(0);
        if fixed_ok != 5 {
            return Some(format!(
                "its destination {qdest}'s fixed chunk columns no longer have their \
                 expected types/identity"
            ));
        }

        // (3) the vector column is pgvector's exact vector(dim).
        let vec_ok = Spi::get_one_with_args::<bool>(
            "SELECT a.atttypid = vt.oid AND a.atttypmod = $3
               FROM pg_attribute a
               CROSS JOIN (SELECT t.oid FROM pg_type t
                             JOIN pg_extension e ON e.extnamespace = t.typnamespace
                            WHERE e.extname = 'vector' AND t.typname = 'vector') vt
              WHERE a.attrelid = $1 AND a.attname = $2
                AND a.attnum > 0 AND NOT a.attisdropped",
            &[
                dest_oid.into(),
                self.vector_column.as_str().into(),
                self.dim.into(),
            ],
        )
        .unwrap_or(Some(false))
        .unwrap_or(false);
        if !vec_ok {
            return Some(format!(
                "its destination vector column {:?} is no longer vector({}) on {qdest}",
                self.vector_column, self.dim
            ));
        }

        // (4) the source key clones the live source PK exactly (type OID,
        // typmod, collation) — only checkable while the source exists.
        let src_oid = Spi::get_one_with_args::<pg_sys::Oid>(
            "SELECT to_regclass($1)::oid",
            &[self.qualified_table().as_str().into()],
        )
        .ok()
        .flatten()
        .filter(|o| *o != pg_sys::Oid::INVALID);
        if let Some(src_oid) = src_oid {
            let key_ok = Spi::get_one_with_args::<bool>(
                "SELECT d.atttypid = s.atttypid AND d.atttypmod = s.atttypmod
                        AND d.attcollation = s.attcollation
                   FROM pg_attribute d, pg_attribute s
                  WHERE d.attrelid = $1 AND d.attname = 'postvec_source_pk'
                    AND d.attnum > 0 AND NOT d.attisdropped
                    AND s.attrelid = $2 AND s.attname = $3
                    AND s.attnum > 0 AND NOT s.attisdropped",
                &[
                    dest_oid.into(),
                    src_oid.into(),
                    self.pk_columns[0].as_str().into(),
                ],
            )
            .unwrap_or(Some(false))
            .unwrap_or(false);
            if !key_ok {
                return Some(format!(
                    "its destination source key no longer matches the source primary key's \
                     exact type/typmod/collation on {qdest}"
                ));
            }
        }

        // (5) the (source_pk, chunk_seq) unique key — a real, total,
        // plain-column one: a partial (`WHERE ...`), expression, invalid, or
        // not-ready unique index does not enforce the promised uniqueness.
        let unique_ok = Spi::get_one_with_args::<bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_index i
                  WHERE i.indrelid = $1 AND i.indisunique AND i.indnkeyatts = 2
                    AND i.indisvalid AND i.indisready AND i.indislive
                    AND i.indpred IS NULL AND i.indexprs IS NULL
                    AND (SELECT a.attname FROM pg_attribute a
                          WHERE a.attrelid = i.indrelid AND a.attnum = i.indkey[0])
                        = 'postvec_source_pk'
                    AND (SELECT a.attname FROM pg_attribute a
                          WHERE a.attrelid = i.indrelid AND a.attnum = i.indkey[1])
                        = 'postvec_chunk_seq')",
            &[dest_oid.into()],
        )
        .unwrap_or(Some(false))
        .unwrap_or(false);
        if !unique_ok {
            return Some(format!(
                "its destination {qdest} no longer has a total UNIQUE \
                 (postvec_source_pk, postvec_chunk_seq) key"
            ));
        }

        // (5b) no *foreign* permissive policy may widen destination reads —
        // an extra `FOR SELECT USING (true)` beside the canonical policy
        // would open every chunk (permissive policies OR together).
        // Restrictive policies only narrow visibility and stay allowed.
        // Unlike (5c), this holds whether or not the source still exists.
        let extra_permissive = Spi::get_one_with_args::<bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_policy p
                  WHERE p.polrelid = $1 AND p.polpermissive
                    AND p.polcmd IN ('r', '*')
                    AND p.polname <> 'postvec_source_visible')",
            &[dest_oid.into()],
        )
        .unwrap_or(Some(true))
        .unwrap_or(true);
        if extra_permissive {
            return Some(format!(
                "its destination {qdest} has an additional permissive SELECT/ALL policy \
                 beside postvec_source_visible, widening chunk visibility"
            ));
        }

        // (5c) the SELECT policy behind FORCE RLS must *be* the generated
        // source-visibility gate, not merely depend on the source — a
        // rewritten `USING (true OR EXISTS (... source ...))` keeps the
        // pg_depend edge while allowing everything. The proof compares the
        // whitespace-normalized `pg_get_expr` deparse of `polqual` against
        // the canonical rendering, under a pinned `pg_catalog` search path so
        // qualification is deterministic (identical deparse text under an
        // identical search path round-trips to identical semantics — the
        // dump/restore guarantee). The accepted equality is derived from
        // catalog identity, not a textual `=`: the strategy-3 (equality)
        // same-operand members of the PK index's own btree operator family —
        // rendered `OPERATOR(<schema>.<name>)` outside `pg_catalog`, the way
        // deparse spells them (citext and other user-typed PKs) — with a
        // cast only when the operand type differs from the PK's type, and
        // that operand constrained to the PK's domain-base chain or an
        // implicit *binary* coercion from it (`varchar → text`, castmethod
        // 'b'). Polymorphic btree families (enum_ops/array_ops/record_ops/
        // range_ops carry their equality at `anyenum`/`anyarray`/`record`/
        // `anyrange`/`anymultirange`, which no concrete type reaches through
        // `pg_cast`) are accepted when the pseudo-type matches the PK's
        // *terminal base* type's typtype/typcategory — cast-free for a
        // direct enum/array/range PK, cast to the terminal base for a domain
        // over an array/range/multirange (`(s.id)::bigint[] = …::bigint[]`),
        // and cast-free again for a domain over a composite (`record` is the
        // one family the parser deparses bare even through a domain) —
        // exactly how the parser deparses each. A hand-written cast to a
        // lossy or collapsing type
        // (`::boolean`, `::name`, `bigint → ::integer`) resolves through a
        // conversion function, not a relabel, and cannot pass. Permissive
        // posture, PUBLIC roles, and no WITH CHECK are proven alongside. Only
        // checkable while the source exists — a prior source `DROP … CASCADE`
        // legitimately removed the policy with it, and FORCE RLS with no
        // policy denies everything, which is the safe direction.
        let policy_src: Option<pg_sys::Oid> = Spi::get_one_with_args::<pg_sys::Oid>(
            "SELECT to_regclass($1)::oid",
            &[self.qualified_table().as_str().into()],
        )
        .ok()
        .flatten()
        .filter(|o| *o != pg_sys::Oid::INVALID);
        if let Some(policy_src) = policy_src {
            let policy_ok = crate::api::registry::with_pinned_search_path(|| {
                Spi::get_one_with_args::<bool>(
                    "WITH RECURSIVE names AS (
                         SELECT quote_ident(sn.nspname) || '.' || quote_ident(sc.relname)
                                    AS src,
                                (SELECT quote_ident(dc.relname) FROM pg_class dc
                                  WHERE dc.oid = $1) AS dest,
                                quote_ident(a.attname) AS pk,
                                a.atttypid
                           FROM pg_class sc
                           JOIN pg_namespace sn ON sn.oid = sc.relnamespace
                           JOIN pg_index i ON i.indrelid = sc.oid AND i.indisprimary
                           JOIN pg_attribute a ON a.attrelid = sc.oid
                                              AND a.attnum = i.indkey[0]
                          WHERE sc.oid = $2
                     ),
                     base AS (
                         SELECT t.oid, t.typbasetype
                           FROM pg_type t JOIN names n ON t.oid = n.atttypid
                          UNION ALL
                         SELECT t.oid, t.typbasetype
                           FROM pg_type t JOIN base b ON t.oid = b.typbasetype
                     ),
                     ops AS (
                         SELECT CASE WHEN opn.nspname = 'pg_catalog' THEN op.oprname
                                     ELSE 'OPERATOR(' || quote_ident(opn.nspname)
                                          || '.' || op.oprname || ')'
                                END AS op_token,
                                CASE WHEN ao.amoplefttype = n.atttypid THEN NULL
                                     -- record differs from the other
                                     -- polymorphic families: a domain over a
                                     -- composite deparses bare, with no
                                     -- terminal-base cast.
                                     WHEN ao.amoplefttype = 'record'::regtype THEN NULL
                                     WHEN ao.amoplefttype IN
                                          ('anyenum'::regtype, 'anyarray'::regtype,
                                           'anyrange'::regtype,
                                           'anymultirange'::regtype) THEN
                                          CASE WHEN term.oid = n.atttypid THEN NULL
                                               ELSE format_type(term.oid, NULL)
                                          END
                                     ELSE format_type(ao.amoplefttype, NULL)
                                END AS cast_t
                           FROM names n
                           JOIN pg_type term
                                ON term.oid = (SELECT b.oid FROM base b
                                                WHERE b.typbasetype = 0)
                           JOIN pg_index i ON i.indrelid = $2 AND i.indisprimary
                           JOIN pg_opclass oc ON oc.oid = i.indclass[0]
                           JOIN pg_amop ao ON ao.amopfamily = oc.opcfamily
                                          AND ao.amopstrategy = 3
                                          AND ao.amoplefttype = ao.amoprighttype
                           JOIN pg_operator op ON op.oid = ao.amopopr
                           JOIN pg_namespace opn ON opn.oid = op.oprnamespace
                          WHERE ao.amoplefttype IN (SELECT b.oid FROM base b)
                             OR (ao.amoplefttype = 'anyenum'::regtype
                                 AND term.typtype = 'e')
                             OR (ao.amoplefttype = 'anyarray'::regtype
                                 AND term.typcategory = 'A')
                             OR (ao.amoplefttype = 'record'::regtype
                                 AND term.typtype = 'c')
                             OR (ao.amoplefttype = 'anyrange'::regtype
                                 AND term.typtype = 'r')
                             OR (ao.amoplefttype = 'anymultirange'::regtype
                                 AND term.typtype = 'm')
                             OR EXISTS (
                                SELECT 1 FROM pg_cast pc
                                 WHERE pc.castsource = term.oid
                                   AND pc.casttarget = ao.amoplefttype
                                   AND pc.castcontext = 'i'
                                   AND pc.castmethod = 'b')
                     ),
                     pol AS (
                         SELECT regexp_replace(
                                    pg_get_expr(p.polqual, p.polrelid), '\\s+', ' ', 'g')
                                    AS qual
                           FROM pg_policy p
                          WHERE p.polrelid = $1
                            AND p.polname = 'postvec_source_visible'
                            AND p.polcmd = 'r'
                            AND p.polpermissive
                            AND p.polroles = ARRAY[0]::oid[]
                            AND p.polwithcheck IS NULL
                            AND EXISTS (
                                SELECT 1 FROM pg_depend d
                                 WHERE d.classid = 'pg_policy'::regclass
                                   AND d.objid = p.oid
                                   AND d.refclassid = 'pg_class'::regclass
                                   AND d.refobjid = $2)
                     )
                     SELECT EXISTS (
                         SELECT 1 FROM pol, names n, ops o
                          WHERE (o.cast_t IS NULL AND pol.qual = format(
                                    '(EXISTS ( SELECT 1 FROM %s s WHERE (s.%s %s \
                                     %s.postvec_source_pk)))',
                                    n.src, n.pk, o.op_token, n.dest))
                             OR (o.cast_t IS NOT NULL AND pol.qual = format(
                                    '(EXISTS ( SELECT 1 FROM %s s WHERE ((s.%s)::%s %s \
                                     (%s.postvec_source_pk)::%s)))',
                                    n.src, n.pk, o.cast_t, o.op_token, n.dest, o.cast_t)))",
                    &[dest_oid.into(), policy_src.into()],
                )
                .unwrap_or(Some(false))
                .unwrap_or(false)
            });
            if !policy_ok {
                return Some(format!(
                    "its destination {qdest}'s source-visibility SELECT policy is missing \
                     or no longer the generated source-visibility gate"
                ));
            }
        }

        // (6) the join view: required while the source exists; when present
        // it must be a view with the exact marker, depending on the
        // destination (and the source while that exists).
        let view_facts: Option<(String, Option<String>, pg_sys::Oid)> = Spi::connect(|c| {
            let t = c
                .select(
                    "SELECT c.relkind::text, obj_description(c.oid, 'pg_class'), c.oid
                       FROM pg_class c
                      WHERE c.oid = to_regclass($1)",
                    Some(1),
                    &[qview.as_str().into()],
                )
                .ok()?;
            t.into_iter().next().map(|r| {
                (
                    r.get::<String>(1).unwrap().unwrap(),
                    r.get::<String>(2).unwrap(),
                    r.get::<pg_sys::Oid>(3).unwrap().unwrap(),
                )
            })
        });
        match view_facts {
            None => {
                if src_oid.is_some() {
                    return Some(format!(
                        "its join view {qview} is gone while the source still exists"
                    ));
                }
                // Source gone: normal dependency handling removed the view.
            }
            Some((vkind, vcomment, view_oid)) => {
                if vkind != "v" {
                    return Some(format!("{qview} is not a view (relkind={vkind})"));
                }
                if vcomment.as_deref() != Some(expected_view_comment.as_str()) {
                    return Some(format!(
                        "{qview} does not carry the entry's exact ownership marker"
                    ));
                }
                // The view is only safe to expose because it evaluates as
                // the CALLER (source RLS applies) behind a barrier; losing
                // either option silently widens what its readers see.
                let opts_ok = Spi::get_one_with_args::<bool>(
                    "SELECT COALESCE(
                                reloptions @> ARRAY['security_invoker=true']
                                AND reloptions @> ARRAY['security_barrier=true'],
                                false)
                       FROM pg_class WHERE oid = $1",
                    &[view_oid.into()],
                )
                .unwrap_or(Some(false))
                .unwrap_or(false);
                if !opts_ok {
                    return Some(format!(
                        "{qview} no longer has security_invoker=true and \
                         security_barrier=true"
                    ));
                }
                let expected_deps: i64 = if src_oid.is_some() { 2 } else { 1 };
                let deps = Spi::get_one_with_args::<i64>(
                    "SELECT count(DISTINCT d.refobjid)
                       FROM pg_depend d
                       JOIN pg_rewrite rw ON rw.oid = d.objid
                      WHERE rw.ev_class = $1
                        AND d.classid = 'pg_rewrite'::regclass
                        AND d.refclassid = 'pg_class'::regclass
                        AND d.refobjid IN ($2, $3)",
                    &[
                        view_oid.into(),
                        dest_oid.into(),
                        src_oid.unwrap_or(pg_sys::Oid::INVALID).into(),
                    ],
                )
                .unwrap_or(Some(0))
                .unwrap_or(0);
                if deps < expected_deps {
                    return Some(format!(
                        "{qview} does not depend on the expected source/destination \
                         relations — it is not the view postvec generated"
                    ));
                }
            }
        }

        // (7) extra (migration scratch) columns.
        for col in extra_columns {
            let exists = Spi::get_one_with_args::<bool>(
                "SELECT EXISTS (
                     SELECT 1 FROM pg_attribute
                      WHERE attrelid = $1 AND attname = $2
                        AND attnum > 0 AND NOT attisdropped)",
                &[dest_oid.into(), (*col).into()],
            )
            .unwrap_or(Some(false))
            .unwrap_or(false);
            if !exists {
                return Some(format!(
                    "destination column {col:?} no longer exists on {qdest}"
                ));
            }
        }
        None
    }

    /// Distinct columns the entry's current template references — the source
    /// column alone when no template is set — in first-occurrence order.
    /// These drive the enqueue-trigger change set and the dependency check.
    ///
    /// For a recursive entry the reserved `chunk` pseudo-column is excluded
    /// (it resolves to destination `chunk_text`, not a source column;
    /// leaving it in would make `missing_dependency()` quarantine every
    /// chunked entry with a template). The source column is always
    /// included: it drives splitting, so its changes must fire the
    /// invalidation triggers even though a recursive template never
    /// references it directly.
    pub fn referenced_columns(&self) -> Result<Vec<String>, FormatError> {
        match self.format.as_deref() {
            None => Ok(vec![self.source_column.clone()]),
            Some(t) => {
                let refs = format_referenced_columns(&parse_format(t)?);
                if self.is_recursive() {
                    let mut out = vec![self.source_column.clone()];
                    for c in refs {
                        if c != "chunk" && !out.contains(&c) {
                            out.push(c);
                        }
                    }
                    Ok(out)
                } else {
                    Ok(refs)
                }
            }
        }
    }

    /// Whether the exact `postvec_trunc_<id>` sentinel trigger is absent from
    /// the (existing) relation.
    fn truncate_sentinel_missing(&self) -> bool {
        let n = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pg_trigger
              WHERE tgrelid = to_regclass($1) AND tgname = $2",
            &[
                self.qualified_table().as_str().into(),
                format!("postvec_trunc_{}", self.id).into(),
            ],
        )
        .unwrap_or(Some(0))
        .unwrap_or(0);
        n < 1
    }

    /// Whether the entry's generated triggers are gone from its (existing)
    /// relation — the signature of a dropped-and-recreated table, whose
    /// registry row is stale even though the names all resolve again. An
    /// observed entry (`trigger_mode = 'none'`) is expected to carry exactly
    /// the TRUNCATE sentinel and nothing else.
    pub fn triggers_missing(&self) -> bool {
        if self.trigger_mode == "none" {
            return self.truncate_sentinel_missing();
        }
        if self.is_recursive() {
            // A recursive entry also carries the DELETE trigger (deletions
            // must purge the document's chunks), so its expected set is
            // five, not three.
            let n = Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgrelid = to_regclass($1)
                    AND tgname IN ($2, $3, $4, $5, $6)",
                &[
                    self.qualified_table().as_str().into(),
                    format!("postvec_ins_{}", self.id).into(),
                    format!("postvec_upd_{}", self.id).into(),
                    format!("postvec_pk_{}", self.id).into(),
                    format!("postvec_del_{}", self.id).into(),
                    format!("postvec_trunc_{}", self.id).into(),
                ],
            )
            .unwrap_or(Some(0))
            .unwrap_or(0);
            return n < 5;
        }
        let n = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM pg_trigger
              WHERE tgrelid = to_regclass($1)
                AND tgname IN ($2, $3, $4)",
            &[
                self.qualified_table().as_str().into(),
                format!("postvec_ins_{}", self.id).into(),
                format!("postvec_upd_{}", self.id).into(),
                format!("postvec_trunc_{}", self.id).into(),
            ],
        )
        .unwrap_or(Some(0))
        .unwrap_or(0);
        n < 3
    }

    fn from_row(row: &pgrx::spi::SpiHeapTupleData) -> Self {
        RegistryEntry {
            id: row.get::<i64>(1).unwrap().unwrap(),
            table_schema: row.get::<String>(2).unwrap().unwrap(),
            table_name: row.get::<String>(3).unwrap().unwrap(),
            source_column: row.get::<String>(4).unwrap().unwrap(),
            vector_column: row.get::<String>(5).unwrap().unwrap(),
            pk_columns: row.get::<Vec<String>>(6).unwrap().unwrap(),
            pk_types: row.get::<Vec<String>>(7).unwrap().unwrap(),
            model: row.get::<String>(8).unwrap().unwrap(),
            dim: row.get::<i32>(9).unwrap().unwrap(),
            fts_config: row.get::<String>(10).unwrap().unwrap(),
            distance: row.get::<String>(11).unwrap().unwrap(),
            backfill_mode: row.get::<String>(12).unwrap().unwrap(),
            backfill_watermark: row.get::<String>(13).unwrap(),
            state: row.get::<String>(14).unwrap().unwrap(),
            trigger_mode: row.get::<String>(15).unwrap().unwrap(),
            owns_vector_column: row.get::<bool>(16).unwrap().unwrap(),
            format: row.get::<String>(17).unwrap(),
            index_mode: row.get::<String>(18).unwrap().unwrap(),
            index_error: row.get::<String>(19).unwrap(),
            chunking: row.get::<String>(20).unwrap().unwrap(),
            chunk_size: row.get::<i32>(21).unwrap(),
            chunk_overlap: row.get::<i32>(22).unwrap(),
            destination_schema: row.get::<String>(23).unwrap(),
            destination_table: row.get::<String>(24).unwrap(),
            destination_view: row.get::<String>(25).unwrap(),
            destination_token: row.get::<String>(26).unwrap(),
        }
    }

    // New fields are appended at the end so existing column indices in
    // from_row stay untouched.
    const SELECT: &'static str = "SELECT id, table_schema, table_name, source_column,
                vector_column, pk_columns, pk_types, model, dim,
                fts_config::text, distance, backfill_mode, backfill_watermark,
                state, trigger_mode, owns_vector_column, format, index_mode,
                index_error, chunking, chunk_size, chunk_overlap,
                destination_schema, destination_table, destination_view,
                destination_token
           FROM postvec.registry";

    /// Load by registry id (any state). Must be called inside a transaction.
    pub fn load(id: i64) -> Option<RegistryEntry> {
        let q = format!("{} WHERE id = $1", Self::SELECT);
        Spi::connect(|client| {
            let table = client.select(q.as_str(), Some(1), &[id.into()]).ok()?;
            table
                .into_iter()
                .next()
                .map(|row| RegistryEntry::from_row(&row))
        })
    }

    /// Load the active entry for a (schema, table, source column), if any.
    pub fn load_active(schema: &str, table: &str, column: &str) -> Option<RegistryEntry> {
        Self::load_by_cols(schema, table, column, true)
    }

    /// Load the entry for a (schema, table, source column) regardless of state.
    pub fn load_any(schema: &str, table: &str, column: &str) -> Option<RegistryEntry> {
        Self::load_by_cols(schema, table, column, false)
    }

    fn load_by_cols(
        schema: &str,
        table: &str,
        column: &str,
        active_only: bool,
    ) -> Option<RegistryEntry> {
        let q = format!(
            "{} WHERE table_schema = $1 AND table_name = $2 AND source_column = $3{}",
            Self::SELECT,
            if active_only {
                " AND state <> 'disabled'"
            } else {
                ""
            }
        );
        Spi::connect(|client| {
            let table = client
                .select(
                    q.as_str(),
                    Some(1),
                    &[schema.into(), table.into(), column.into()],
                )
                .ok()?;
            table
                .into_iter()
                .next()
                .map(|row| RegistryEntry::from_row(&row))
        })
    }
}

/// Always-quote an identifier (equivalent to `quote_ident`, but unconditional):
/// wrap in double quotes and double any embedded quote. Safe to apply to
/// identifiers that would not otherwise need quoting.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// pgvector distance operator for a registry entry's metric.
/// Ascending order of any of these puts the nearest neighbour first.
pub fn distance_op(distance: &str) -> &'static str {
    match distance {
        "l2" => "<->",
        "ip" => "<#>",
        _ => "<=>", // cosine
    }
}

/// pgvector index opclass matching [`distance_op`].
pub fn distance_opclass(distance: &str) -> &'static str {
    match distance {
        "l2" => "vector_l2_ops",
        "ip" => "vector_ip_ops",
        _ => "vector_cosine_ops", // cosine
    }
}

/// The halfvec opclass for a metric — what the recommended expression index
/// uses above pgvector's 2000-dim HNSW limit.
pub fn halfvec_opclass(distance: &str) -> &'static str {
    match distance {
        "l2" => "halfvec_l2_ops",
        "ip" => "halfvec_ip_ops",
        _ => "halfvec_cosine_ops", // cosine
    }
}

/// The opclass an index must use to actually serve `search()` for an entry:
/// `search()` renders its semantic leg through halfvec above pgvector's
/// 2000-dim HNSW limit, plain vector otherwise, so a usable-for-this-entry
/// index is dimension- as well as distance-dependent.
pub fn expected_ann_opclass(distance: &str, dim: i32) -> &'static str {
    if dim > 2000 {
        halfvec_opclass(distance)
    } else {
        distance_opclass(distance)
    }
}

/// `EXISTS (...)` probe: does any **ANN search** index on `{rel}` depend on
/// column `{col}`? Covers direct column indexes (`hnsw (col vector_*_ops)`)
/// via `indkey`, and expression indexes such as the high-dimensional halfvec
/// recommendation (`hnsw ((col::halfvec(N)) halfvec_*_ops)`) via `pg_depend`
/// — Postgres records a per-column dependency for every column an index
/// expression references (the same edges `DROP COLUMN` cascades along), which
/// is exact: no string matching against the expression, so a sibling column
/// like `<col>_new` can never false-positive. The index's access method must
/// be `hnsw` or `ivfflat` (pgvector's ANN AMs) — a btree/gin diagnostic index
/// that merely references the column is not a vector-search index and must
/// not satisfy `status().has_vector_index` or migration finalization.
///
/// `rel` and `col` are SQL *expressions* interpolated verbatim (bind
/// parameters or catalog column references) — the single fragment shared by
/// [`vector_index_exists`] and `status()`'s set-based query.
pub(crate) fn vector_index_probe_sql(rel: &str, col: &str) -> String {
    format!(
        "EXISTS (
             SELECT 1
               FROM pg_index i
               JOIN pg_class ic ON ic.oid = i.indexrelid
               JOIN pg_am am ON am.oid = ic.relam
              WHERE i.indrelid = {rel}
                AND am.amname IN ('hnsw', 'ivfflat')
                -- a failed CREATE INDEX CONCURRENTLY leaves an invalid index
                -- PostgreSQL will not use; it must not satisfy status(),
                -- migration finalization, or the adopt-time advisory
                AND i.indisvalid AND i.indisready AND i.indislive
                AND (
                    EXISTS (
                        SELECT 1
                          FROM pg_attribute a
                         WHERE a.attrelid = i.indrelid
                           AND a.attnum = ANY(i.indkey)
                           AND a.attname = {col}
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
                           AND a.attname = {col}
                    )
                )
        )"
    )
}

/// Whether any index depends on the vector column (see
/// [`vector_index_probe_sql`]).
pub fn vector_index_exists(qualified_table: &str, vector_column: &str) -> bool {
    let q = format!("SELECT {}", vector_index_probe_sql("to_regclass($1)", "$2"));
    Spi::get_one_with_args::<bool>(&q, &[qualified_table.into(), vector_column.into()])
        .unwrap()
        .unwrap_or(false)
}

/// Quote a string as a SQL literal (`quote_literal`): wrap in single quotes and
/// double any embedded single quote.
///
/// NOT sufficient for text that later runs under an unknown
/// `standard_conforming_strings` setting — stored user input that will be
/// interpolated into worker-executed SQL must go through
/// [`quote_literal_estring`] instead.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quote a string as a setting-independent `E'...'` literal, escaping both
/// backslashes and apostrophes. This is the only literal form that renders
/// identically under `standard_conforming_strings = on` and `off`, which is
/// mandatory for stored user input (template literals) that the superuser
/// worker later interpolates into dynamic SQL.
pub fn quote_literal_estring(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    out.push_str("E'");
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

// ---- Formatting / context templates -----------------------------------

/// Template shape limits (query-shape bounds, not configuration).
pub const FORMAT_MAX_BYTES: usize = 16 * 1024;
pub const FORMAT_MAX_REFS: usize = 64;

/// One parsed template segment. Adjacent literals are coalesced by
/// [`parse_format`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatSeg {
    Literal(String),
    Column(String),
}

/// A template parse error, with the byte position it was detected at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatError {
    pub pos: usize,
    pub msg: String,
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at byte {})", self.msg, self.pos)
    }
}

/// Parse a format template. The grammar is deliberately small:
///
/// - `$name` for an ASCII simple identifier (`[A-Za-z_][A-Za-z0-9_]*`);
/// - `${exact column name}` for every other identifier (`}}` inside the
///   braces decodes to one literal `}` in the column name);
/// - `$$` for a literal dollar sign;
/// - every other byte is literal. There is **no** backslash-escape
///   processing: `\n` is two literal characters — newlines enter a template
///   through SQL string syntax (`E'\n'` or a literal newline).
pub fn parse_format(template: &str) -> Result<Vec<FormatSeg>, FormatError> {
    if template.is_empty() {
        return Err(FormatError {
            pos: 0,
            msg: "the template is empty (use NULL to clear a template)".into(),
        });
    }
    if template.len() > FORMAT_MAX_BYTES {
        return Err(FormatError {
            pos: FORMAT_MAX_BYTES,
            msg: format!(
                "the template is {} bytes (max {FORMAT_MAX_BYTES})",
                template.len()
            ),
        });
    }
    let mut segs: Vec<FormatSeg> = Vec::new();
    let mut lit = String::new();
    let mut chars = template.char_indices().peekable();
    let push_column = |segs: &mut Vec<FormatSeg>, lit: &mut String, name: String| {
        if !lit.is_empty() {
            segs.push(FormatSeg::Literal(std::mem::take(lit)));
        }
        segs.push(FormatSeg::Column(name));
    };
    while let Some((pos, ch)) = chars.next() {
        if ch != '$' {
            lit.push(ch);
            continue;
        }
        match chars.peek().copied() {
            Some((_, '$')) => {
                chars.next();
                lit.push('$');
            }
            Some((_, c)) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some((_, c)) = chars.peek().copied() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                push_column(&mut segs, &mut lit, name);
            }
            Some((brace_pos, '{')) => {
                chars.next();
                let mut name = String::new();
                let mut closed = false;
                while let Some((_, c)) = chars.next() {
                    if c == '}' {
                        // `}}` decodes to one literal `}` in the name;
                        // a single `}` closes the reference.
                        if matches!(chars.peek(), Some((_, '}'))) {
                            chars.next();
                            name.push('}');
                        } else {
                            closed = true;
                            break;
                        }
                    } else {
                        name.push(c);
                    }
                }
                if !closed {
                    return Err(FormatError {
                        pos: brace_pos,
                        msg: "unclosed ${...} column reference".into(),
                    });
                }
                if name.is_empty() {
                    return Err(FormatError {
                        pos: brace_pos,
                        msg: "empty ${} column reference".into(),
                    });
                }
                push_column(&mut segs, &mut lit, name);
            }
            _ => {
                return Err(FormatError {
                    pos,
                    msg: "dangling '$' (use '$$' for a literal dollar sign, $name or \
                          ${name} for a column reference)"
                        .into(),
                });
            }
        }
    }
    if !lit.is_empty() {
        segs.push(FormatSeg::Literal(lit));
    }
    let distinct = format_referenced_columns(&segs);
    if distinct.len() > FORMAT_MAX_REFS {
        return Err(FormatError {
            pos: 0,
            msg: format!(
                "{} distinct columns referenced (max {FORMAT_MAX_REFS})",
                distinct.len()
            ),
        });
    }
    Ok(segs)
}

/// Distinct referenced columns, deduplicated in first-occurrence order.
pub fn format_referenced_columns(segs: &[FormatSeg]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in segs {
        if let FormatSeg::Column(c) = seg {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
    }
    out
}

/// The one render expression for all document-embedding reads (queue source
/// reads and the reembed migration leg). With no template it returns exactly
/// today's raw `source::text` expression; with a template it renders
///
/// ```sql
/// CASE WHEN alias.source IS NOT NULL THEN
///     E'literal' || COALESCE(alias.col::text, '') || ...
/// END
/// ```
///
/// The CASE is the lifecycle anchor: context columns cannot keep a vector
/// alive after the source becomes NULL (the expression goes NULL, the job
/// takes the no-inference NULL path, the vector converges to NULL). Literal
/// segments always render through [`quote_literal_estring`] — they are stored
/// user input that the superuser worker interpolates into dynamic SQL, so the
/// apostrophe-only [`quote_literal`] would be an insufficient boundary under
/// `standard_conforming_strings = off`.
pub fn format_expr(entry: &RegistryEntry, alias: &str) -> String {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let Some(template) = entry.format.as_deref() else {
        return format!("{src}::text");
    };
    let segs = parse_format(template).unwrap_or_else(|e| {
        // Stored templates are validated before storage; an invalid one means
        // the registry row was edited by hand. Fail loudly rather than embed
        // the wrong text (a plain panic, which pgrx reports as an ERROR in a
        // backend, keeps this function linkable from non-postgres unit tests).
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let parts: Vec<String> = segs
        .iter()
        .map(|seg| match seg {
            FormatSeg::Literal(s) => quote_literal_estring(s),
            FormatSeg::Column(c) => {
                format!("COALESCE({prefix}{}::text, '')", quote_ident(c))
            }
        })
        .collect();
    format!(
        "CASE WHEN {src} IS NOT NULL THEN {} END",
        parts.join(" || ")
    )
}

/// Chunk-mode document-embedding render expression: the recursive
/// counterpart of [`format_expr`], with a source alias for context columns
/// and a chunk alias for the reserved `$chunk` pseudo-column.
///
/// With no template it renders exactly `chunk_alias.chunk_text` (the chunk is
/// embedded verbatim). With a template it renders the same CASE-anchored
/// concatenation as column mode, except that `Column("chunk")` resolves to
/// the destination's `chunk_text` (NOT NULL, so no COALESCE) and every other
/// reference resolves against the source alias. The CASE anchor keeps the
/// lifecycle rule: a NULL source renders SQL NULL whatever the context holds
/// — the claim path treats that as an obsolete chunk, never as embeddable
/// text.
pub fn chunk_format_expr(entry: &RegistryEntry, source_alias: &str, chunk_alias: &str) -> String {
    let chunk = format!("{chunk_alias}.chunk_text");
    let Some(template) = entry.format.as_deref() else {
        return chunk;
    };
    let prefix = if source_alias.is_empty() {
        String::new()
    } else {
        format!("{source_alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let segs = parse_format(template).unwrap_or_else(|e| {
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let parts: Vec<String> = segs
        .iter()
        .map(|seg| match seg {
            FormatSeg::Literal(s) => quote_literal_estring(s),
            FormatSeg::Column(c) if c == "chunk" => chunk.clone(),
            FormatSeg::Column(c) => {
                format!("COALESCE({prefix}{}::text, '')", quote_ident(c))
            }
        })
        .collect();
    format!(
        "CASE WHEN {src} IS NOT NULL THEN {} END",
        parts.join(" || ")
    )
}

/// The EXACT byte length [`format_expr`] would render, computed **without
/// building the concatenation**: literal byte lengths are summed as a
/// constant and each column reference contributes
/// `octet_length(COALESCE(col::text, ''))`. Concatenation length equals the
/// sum of its parts, so the value matches `octet_length(format_expr(...))`
/// byte for byte — while `octet_length` on a toasted text column reads the
/// stored size without detoasting. This is what lets the claim/migration
/// reads measure every candidate row cheaply and render ONLY the rows that
/// pass the byte ceilings (the render itself stays in a lazily-evaluated
/// projection over the admitted set).
pub fn format_len_expr(entry: &RegistryEntry, alias: &str) -> String {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let Some(template) = entry.format.as_deref() else {
        return format!("octet_length({src}::text)::bigint");
    };
    let segs = parse_format(template).unwrap_or_else(|e| {
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let mut literal_bytes: i64 = 0;
    let mut terms: Vec<String> = Vec::new();
    for seg in &segs {
        match seg {
            FormatSeg::Literal(s) => literal_bytes += s.len() as i64,
            FormatSeg::Column(c) => terms.push(format!(
                "COALESCE(octet_length({prefix}{}::text)::bigint, 0)",
                quote_ident(c)
            )),
        }
    }
    let mut sum = format!("{literal_bytes}::bigint");
    for t in terms {
        sum.push_str(" + ");
        sum.push_str(&t);
    }
    format!("CASE WHEN {src} IS NOT NULL THEN {sum} END")
}

/// The chunk-mode twin of [`format_len_expr`], mirroring
/// [`chunk_format_expr`]: `$chunk` contributes `octet_length(chunk_text)`
/// (NOT NULL by schema), everything else resolves against the source alias.
pub fn chunk_format_len_expr(
    entry: &RegistryEntry,
    source_alias: &str,
    chunk_alias: &str,
) -> String {
    let chunk_len = format!("octet_length({chunk_alias}.chunk_text)::bigint");
    let Some(template) = entry.format.as_deref() else {
        return chunk_len;
    };
    let prefix = if source_alias.is_empty() {
        String::new()
    } else {
        format!("{source_alias}.")
    };
    let src = format!("{prefix}{}", quote_ident(&entry.source_column));
    let segs = parse_format(template).unwrap_or_else(|e| {
        panic!(
            "postvec: stored format template for registry id {} is invalid: {e}",
            entry.id
        )
    });
    let mut literal_bytes: i64 = 0;
    let mut terms: Vec<String> = Vec::new();
    for seg in &segs {
        match seg {
            FormatSeg::Literal(s) => literal_bytes += s.len() as i64,
            FormatSeg::Column(c) if c == "chunk" => terms.push(chunk_len.clone()),
            FormatSeg::Column(c) => terms.push(format!(
                "COALESCE(octet_length({prefix}{}::text)::bigint, 0)",
                quote_ident(c)
            )),
        }
    }
    let mut sum = format!("{literal_bytes}::bigint");
    for t in terms {
        sum.push_str(" + ");
        sum.push_str(&t);
    }
    format!("CASE WHEN {src} IS NOT NULL THEN {sum} END")
}

/// Serialize an embedding to pgvector's text input format: `[f1,f2,...]`.
/// Cast the result server-side with `$n::vector`.
pub fn serialize_vector(v: &[f32]) -> String {
    let mut out = String::with_capacity(v.len() * 8 + 2);
    out.push('[');
    for (i, f) in v.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // Rust's shortest round-tripping float formatting; pgvector parses it.
        out.push_str(&f.to_string());
    }
    out.push(']');
    out
}

/// Parse pgvector's text output format (`[f1,f2,...]`, as produced by
/// `vec::text`) back into floats — the migration driver reads stored vectors
/// this way before sending them to `ConvertEmbeddings`.
pub fn parse_vector(text: &str) -> Result<Vec<f32>, String> {
    let inner = text
        .trim()
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| format!("not a pgvector text literal: {text:?}"))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|f| {
            f.trim()
                .parse::<f32>()
                .map_err(|e| format!("bad float {f:?} in vector: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pk_columns: &[&str], pk_types: &[&str]) -> RegistryEntry {
        RegistryEntry {
            id: 1,
            table_schema: "public".into(),
            table_name: "docs".into(),
            source_column: "body".into(),
            vector_column: "body_semantic".into(),
            pk_columns: pk_columns.iter().map(|s| s.to_string()).collect(),
            pk_types: pk_types.iter().map(|s| s.to_string()).collect(),
            model: "m".into(),
            dim: 3,
            fts_config: "pg_catalog.english".into(),
            distance: "cosine".into(),
            backfill_mode: "none".into(),
            backfill_watermark: None,
            state: "active".into(),
            trigger_mode: "statement".into(),
            owns_vector_column: true,
            format: None,
            index_mode: "manual".into(),
            index_error: None,
            chunking: "none".into(),
            chunk_size: None,
            chunk_overlap: None,
            destination_schema: None,
            destination_table: None,
            destination_view: None,
            destination_token: None,
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("body"), "\"body\"");
        assert_eq!(quote_ident("we\"ird"), "\"we\"\"ird\"");
        assert_eq!(quote_literal("O'Brien"), "'O''Brien'");
    }

    #[test]
    fn vector_serialization() {
        assert_eq!(serialize_vector(&[]), "[]");
        assert_eq!(serialize_vector(&[0.5, -1.0, 2.0]), "[0.5,-1,2]");
    }

    #[test]
    fn vector_parse_roundtrip() {
        assert_eq!(parse_vector("[]").unwrap(), Vec::<f32>::new());
        assert_eq!(parse_vector("[0.5,-1,2]").unwrap(), vec![0.5f32, -1.0, 2.0]);
        assert_eq!(
            parse_vector(&serialize_vector(&[1.25, -0.001])).unwrap(),
            vec![1.25f32, -0.001]
        );
        assert!(parse_vector("0.5,1").is_err());
        assert!(parse_vector("[a,b]").is_err());
    }

    #[test]
    fn single_pk_expressions() {
        let e = entry(&["id"], &["bigint"]);
        assert!(!e.is_composite_pk());
        assert_eq!(e.pk_text_expr(""), "\"id\"::text");
        assert_eq!(e.pk_text_expr("t"), "t.\"id\"::text");
        assert_eq!(e.pk_any_clause("t", "$1"), "t.\"id\" = ANY($1::bigint[])");
        assert_eq!(
            e.pk_staging_join_clause("t", "d", "pk"),
            "t.\"id\" = d.\"pk\"::bigint"
        );
        assert_eq!(e.pk_order_expr("t"), "t.\"id\"");
        assert_eq!(
            e.pk_watermark_clause("t", "42"),
            " AND t.\"id\" > '42'::bigint"
        );
    }

    // ---- Template parser and render expression ----

    fn lit(s: &str) -> FormatSeg {
        FormatSeg::Literal(s.into())
    }
    fn col(s: &str) -> FormatSeg {
        FormatSeg::Column(s.into())
    }

    #[test]
    fn format_parser_matrix() {
        assert_eq!(parse_format("$body").unwrap(), vec![col("body")]);
        assert_eq!(
            parse_format("a $body b").unwrap(),
            vec![lit("a "), col("body"), lit(" b")]
        );
        assert_eq!(
            parse_format("$title — $author\n\n$body").unwrap(),
            vec![
                col("title"),
                lit(" — "),
                col("author"),
                lit("\n\n"),
                col("body")
            ]
        );
        // Braced names take arbitrary characters; `}}` decodes to one `}`.
        assert_eq!(parse_format("${we ird}").unwrap(), vec![col("we ird")]);
        assert_eq!(parse_format("${a}}b}").unwrap(), vec![col("a}b")]);
        // `$$` is a literal dollar; adjacent literals coalesce.
        assert_eq!(parse_format("$$5 a$$b").unwrap(), vec![lit("$5 a$b")]);
        assert_eq!(parse_format("$a$b").unwrap(), vec![col("a"), col("b")]);
        // Underscore/digit name rules.
        assert_eq!(parse_format("$_x9 ").unwrap(), vec![col("_x9"), lit(" ")]);
        // No backslash processing: \n is two literal characters.
        assert_eq!(parse_format(r"a\nb").unwrap(), vec![lit(r"a\nb")]);
    }

    #[test]
    fn format_parser_errors_carry_positions() {
        assert_eq!(parse_format("").unwrap_err().pos, 0);
        assert_eq!(parse_format("$").unwrap_err().pos, 0);
        assert_eq!(parse_format("abc$").unwrap_err().pos, 3);
        assert_eq!(
            parse_format("$1").unwrap_err().pos,
            0,
            "digit cannot start a name"
        );
        assert_eq!(parse_format("ab${}").unwrap_err().pos, 3);
        assert_eq!(parse_format("ab${xy").unwrap_err().pos, 3);
        assert_eq!(
            parse_format("${a}}b").unwrap_err().pos,
            1,
            "}} consumed as a literal brace leaves the reference unclosed"
        );

        let oversized = "x".repeat(FORMAT_MAX_BYTES + 1);
        assert!(parse_format(&oversized).unwrap_err().msg.contains("bytes"));
        let many: String = (0..65).map(|i| format!("$c{i} ")).collect();
        assert!(parse_format(&many).unwrap_err().msg.contains("distinct"));
    }

    #[test]
    fn format_referenced_columns_dedup_in_first_occurrence_order() {
        let segs = parse_format("$b $a $b $c $a").unwrap();
        assert_eq!(format_referenced_columns(&segs), vec!["b", "a", "c"]);
    }

    #[test]
    fn estring_literal_doubles_backslashes_and_quotes() {
        assert_eq!(quote_literal_estring("plain"), "E'plain'");
        assert_eq!(quote_literal_estring("it's"), r"E'it\'s'");
        assert_eq!(quote_literal_estring(r"a\b"), r"E'a\\b'");
        assert_eq!(quote_literal_estring(r"\'"), r"E'\\\''");
    }

    #[test]
    fn format_expr_golden_sql() {
        let mut e = entry(&["id"], &["bigint"]);
        // No template: exactly today's raw source expression.
        assert_eq!(format_expr(&e, "t"), "t.\"body\"::text");
        assert_eq!(format_expr(&e, ""), "\"body\"::text");

        e.format = Some("$title — $body".into());
        assert_eq!(
            format_expr(&e, "t"),
            "CASE WHEN t.\"body\" IS NOT NULL THEN \
             COALESCE(t.\"title\"::text, '') || E' — ' || COALESCE(t.\"body\"::text, '') END"
        );

        // Hostile literal content renders through the setting-independent
        // E-string helper, never the apostrophe-only quote_literal().
        e.format = Some(r"it's \ ok $body".into());
        assert_eq!(
            format_expr(&e, ""),
            r#"CASE WHEN "body" IS NOT NULL THEN E'it\'s \\ ok ' || COALESCE("body"::text, '') END"#
        );
    }

    #[test]
    fn composite_pk_expressions() {
        let e = entry(&["a", "b"], &["integer", "text"]);
        assert!(e.is_composite_pk());
        assert_eq!(e.pk_text_expr("n"), "ROW(n.\"a\", n.\"b\")::text");
        assert_eq!(
            e.pk_any_clause("t", "$1"),
            "ROW(t.\"a\", t.\"b\")::text = ANY($1)"
        );
        assert_eq!(
            e.pk_staging_join_clause("t", "d", "pk"),
            "ROW(t.\"a\", t.\"b\")::text = d.\"pk\""
        );
        assert_eq!(e.pk_order_expr(""), "ROW(\"a\", \"b\")::text COLLATE \"C\"");
        assert_eq!(
            e.pk_watermark_clause("", "(1,x)"),
            " AND ROW(\"a\", \"b\")::text COLLATE \"C\" > '(1,x)' COLLATE \"C\""
        );
    }
}
