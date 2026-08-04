use pgrx::prelude::*;
pub use postvec_core::registry::*;

pub trait RegistryEntryDb: Sized {
    const SELECT: &'static str = "SELECT id, table_schema, table_name, source_column,
                vector_column, pk_columns, pk_types, model, dim,
                fts_config::text, distance, backfill_mode, backfill_watermark,
                state, trigger_mode, owns_vector_column, format, index_mode,
                index_error, chunking, chunk_size, chunk_overlap,
                destination_schema, destination_table, destination_view,
                destination_token
           FROM postvec.registry";
    fn missing_dependency(&self, extra_columns: &[&str]) -> Option<String>;
    fn missing_dependency_locked(&self, extra_columns: &[&str]) -> Option<String>;
    fn missing_dependency_inner(
        &self,
        extra_columns: &[&str],
        lock_relations: bool,
    ) -> Option<String>;
    fn missing_destination_dependency(&self, extra_columns: &[&str]) -> Option<String>;
    fn missing_destination_dependency_inner(
        &self,
        extra_columns: &[&str],
        lock_relation: bool,
    ) -> Option<String>;
    fn truncate_sentinel_missing(&self) -> bool;
    fn triggers_missing(&self) -> bool;
    fn from_row(row: &pgrx::spi::SpiHeapTupleData) -> Self;
    fn load(id: i64) -> Option<RegistryEntry>;
    fn load_active(schema: &str, table: &str, column: &str) -> Option<RegistryEntry>;
    fn load_any(schema: &str, table: &str, column: &str) -> Option<RegistryEntry>;
    fn load_by_cols(
        schema: &str,
        table: &str,
        column: &str,
        active_only: bool,
    ) -> Option<RegistryEntry>;
}

impl RegistryEntryDb for RegistryEntry {
    fn missing_dependency(&self, extra_columns: &[&str]) -> Option<String> {
        self.missing_dependency_inner(extra_columns, false)
    }
    fn missing_dependency_locked(&self, extra_columns: &[&str]) -> Option<String> {
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
    fn missing_destination_dependency(&self, extra_columns: &[&str]) -> Option<String> {
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
    fn triggers_missing(&self) -> bool {
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
    fn load(id: i64) -> Option<RegistryEntry> {
        let q = format!("{} WHERE id = $1", Self::SELECT);
        Spi::connect(|client| {
            let table = client.select(q.as_str(), Some(1), &[id.into()]).ok()?;
            table
                .into_iter()
                .next()
                .map(|row| RegistryEntry::from_row(&row))
        })
    }
    fn load_active(schema: &str, table: &str, column: &str) -> Option<RegistryEntry> {
        Self::load_by_cols(schema, table, column, true)
    }
    fn load_any(schema: &str, table: &str, column: &str) -> Option<RegistryEntry> {
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
/// Whether any index depends on the vector column (see
/// [`vector_index_probe_sql`]).
pub fn vector_index_exists(qualified_table: &str, vector_column: &str) -> bool {
    let q = format!("SELECT {}", vector_index_probe_sql("to_regclass($1)", "$2"));
    Spi::get_one_with_args::<bool>(&q, &[qualified_table.into(), vector_column.into()])
        .unwrap()
        .unwrap_or(false)
}
