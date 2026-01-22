//! `postvec.search()`: hybrid FTS + pgvector KNN fused with RRF. The query
//! string is embedded synchronously through the backend-side gRPC client
//! (the one place inline network I/O is allowed, bounded by
//! `postvec.query_timeout_ms`). On failure we degrade to FTS-only with a
//! `WARNING` when `postvec.search_degrade_to_fts` is on, else error.
//!
//! All SQL is built with `format('%I'/'%L')`-equivalent quoting. Ranks are
//! fused as `w/(k+rank_sem) + (1-w)/(k+rank_fts)`.

use crate::api::embed::embed_texts;
use crate::api::registry::{resolve_relation, RelInfo};
use crate::gucs;
use crate::registry::{quote_ident, quote_literal, serialize_vector, RegistryEntry};
use pgrx::prelude::*;
use pgrx::JsonB;

/// One fused result row: the four original fields plus nullable
/// winning-chunk metadata (all NULL for a column-mode entry).
type Row = (
    String,
    f64,
    Option<i64>,
    Option<i64>,
    Option<i32>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// Resolve `'schema.table'` for search (no ownership requirement — reading
/// the table through SPI enforces SELECT privilege naturally).
fn search_relation(relation: &str) -> RelInfo {
    resolve_relation(relation)
}

// ---- Typed metadata filters ----------------------------------------
//
// An AND-only, catalog-aware JSON filter pushed into both candidate CTEs
// before ranking and LIMIT. Filters are data, never SQL: column names are
// validated against the live catalog, values are validated against the real
// column type (`pg_input_is_valid`) and passed as numbered SPI bind
// parameters with an explicit cast to the catalog type. The renderer runs
// before `search()`'s query-embedding call, so malformed input never costs an
// inference request.

/// Query-shape limits (not product configuration): serialized filter size,
/// distinct top-level columns, and values in one `in` list.
const FILTER_MAX_BYTES: usize = 64 * 1024;
const FILTER_MAX_COLUMNS: usize = 32;
const FILTER_MAX_IN_VALUES: usize = 256;

/// Ceiling on the summed `chunk_text` bytes one chunked search call may
/// materialize: `limit_n` bounds rows, not bytes, and a winning chunk can
/// carry up to `chunk_size` characters of text. Enforced in three phases —
/// fetch winner LENGTHS only, admit ids under the ceiling in Rust, then
/// fetch text for the admitted ids alone — so neither the database nor the
/// Rust side ever materializes more than the budget (candidate CTEs never
/// project chunk text at all, and admitted strings MOVE into result rows).
/// Rows past the ceiling keep their rank but return `chunk_text` NULL, with
/// a WARNING. Non-configurable safety maximum.
const RESULT_TEXT_MAX_BYTES: usize = 64 * 1024 * 1024;

/// One JSON scalar, canonicalised to its bind-parameter text: strings use
/// their contents, numbers their canonical JSON representation, booleans
/// `true`/`false`. All three then go through the same type-validation path.
struct ScalarVal {
    text: String,
    is_string: bool,
}

fn scalar_val(col: &str, ctx: &str, v: &serde_json::Value) -> ScalarVal {
    use serde_json::Value;
    match v {
        Value::String(s) => ScalarVal {
            text: s.clone(),
            is_string: true,
        },
        Value::Number(n) => ScalarVal {
            text: n.to_string(),
            is_string: false,
        },
        Value::Bool(b) => ScalarVal {
            text: b.to_string(),
            is_string: false,
        },
        Value::Null => error!(
            "postvec: filter: {ctx} for column {col:?} cannot be null (use the scalar null \
             form for IS NULL, or {{\"is_not\": null}} for IS NOT NULL)"
        ),
        Value::Array(_) | Value::Object(_) => error!(
            "postvec: filter: {ctx} for column {col:?} must be a scalar \
             (string, number, or boolean)"
        ),
    }
}

/// One rendered-to-be condition for a column. Multiple conditions for one
/// column compose with AND, like everything else in the filter.
enum Cond {
    Eq(ScalarVal),
    IsNull,
    IsNotNull,
    /// `op` is the SQL operator spelling (`<>`, `>`, `>=`, `<`, `<=`).
    Cmp {
        op: &'static str,
        val: ScalarVal,
    },
    In(Vec<ScalarVal>),
    Like {
        insensitive: bool,
        val: ScalarVal,
    },
}

fn parse_in_list(col: &str, values: &[serde_json::Value]) -> Cond {
    if values.is_empty() {
        error!("postvec: filter: the \"in\" list for column {col:?} is empty");
    }
    if values.len() > FILTER_MAX_IN_VALUES {
        error!(
            "postvec: filter: the \"in\" list for column {col:?} has {} values \
             (max {FILTER_MAX_IN_VALUES})",
            values.len()
        );
    }
    Cond::In(
        values
            .iter()
            .map(|v| scalar_val(col, "an \"in\" value", v))
            .collect(),
    )
}

/// Parse one operator object. The operator set is closed; every shape
/// violation named in the design guide refuses here, before any catalog
/// lookup or inference call.
fn parse_op_object(col: &str, map: &serde_json::Map<String, serde_json::Value>) -> Vec<Cond> {
    use serde_json::Value;
    if map.is_empty() {
        error!("postvec: filter: the operator object for column {col:?} is empty");
    }
    let mut conds = Vec::with_capacity(map.len());
    // serde_json's default map is ordered by key, so the rendered operator
    // order is deterministic regardless of the JSON author's spelling order.
    for (op, v) in map {
        let cond = match op.as_str() {
            "neq" => Cond::Cmp {
                op: "<>",
                val: scalar_val(col, "the \"neq\" value", v),
            },
            "gt" => Cond::Cmp {
                op: ">",
                val: scalar_val(col, "the \"gt\" value", v),
            },
            "gte" => Cond::Cmp {
                op: ">=",
                val: scalar_val(col, "the \"gte\" value", v),
            },
            "lt" => Cond::Cmp {
                op: "<",
                val: scalar_val(col, "the \"lt\" value", v),
            },
            "lte" => Cond::Cmp {
                op: "<=",
                val: scalar_val(col, "the \"lte\" value", v),
            },
            "in" => match v {
                Value::Array(a) => parse_in_list(col, a),
                _ => error!("postvec: filter: \"in\" for column {col:?} must receive an array"),
            },
            "like" | "ilike" => match v {
                Value::String(_) => Cond::Like {
                    insensitive: op == "ilike",
                    val: scalar_val(col, "the pattern", v),
                },
                _ => error!(
                    "postvec: filter: {op:?} for column {col:?} must receive a JSON string \
                     pattern"
                ),
            },
            "is_not" => match v {
                Value::Null => Cond::IsNotNull,
                _ => error!(
                    "postvec: filter: \"is_not\" for column {col:?} accepts only null \
                     (IS NOT NULL); use \"neq\" for value inequality"
                ),
            },
            other => error!(
                "postvec: filter: unknown operator {other:?} for column {col:?} (supported: \
                 neq, gt, gte, lt, lte, in, like, ilike, is_not)"
            ),
        };
        conds.push(cond);
    }
    conds
}

/// Parse the whole filter into per-column condition lists. JSON keys are
/// PostgreSQL column names as written — never case-folded or fuzzy-matched.
/// Column order is deterministic (serde_json's ordered map).
fn parse_filter(filter: &serde_json::Value) -> Vec<(String, Vec<Cond>)> {
    use serde_json::Value;
    let Value::Object(map) = filter else {
        error!("postvec: filter: the top level must be a JSON object of column conditions");
    };
    if map.len() > FILTER_MAX_COLUMNS {
        error!(
            "postvec: filter: {} columns referenced (max {FILTER_MAX_COLUMNS})",
            map.len()
        );
    }
    map.iter()
        .map(|(col, v)| {
            let conds = match v {
                Value::Null => vec![Cond::IsNull],
                Value::String(_) | Value::Number(_) | Value::Bool(_) => {
                    vec![Cond::Eq(scalar_val(col, "the equality value", v))]
                }
                Value::Array(a) => vec![parse_in_list(col, a)],
                Value::Object(m) => parse_op_object(col, m),
            };
            (col.clone(), conds)
        })
        .collect()
}

/// The catalog facts one referenced column needs: its exact declared type
/// (the cast/validation target, typmod included) and the category of the
/// effective base type behind any domain (the string-category gate for
/// like/ilike). Operator resolution is NOT approximated from the catalog —
/// [`assert_operator`] asks PostgreSQL's parser directly.
struct ColumnMeta {
    declared: String,
    typcategory: String,
}

fn column_meta(rel_oid: pg_sys::Oid, col: &str) -> ColumnMeta {
    let found: Option<(String, String)> = Spi::connect(|c| {
        let t = c
            .select(
                "WITH RECURSIVE att AS (
                     SELECT a.atttypid,
                            pg_catalog.format_type(a.atttypid, a.atttypmod) AS declared
                       FROM pg_attribute a
                      WHERE a.attrelid = $1 AND a.attname = $2
                        AND a.attnum > 0 AND NOT a.attisdropped
                 ), walk AS (
                     SELECT t.oid, t.typbasetype, t.typcategory
                       FROM pg_type t JOIN att ON t.oid = att.atttypid
                     UNION ALL
                     SELECT t.oid, t.typbasetype, t.typcategory
                       FROM pg_type t JOIN walk w ON t.oid = w.typbasetype
                 )
                 SELECT att.declared, w.typcategory::text
                   FROM att, walk w WHERE w.typbasetype = 0",
                Some(1),
                &[rel_oid.into(), col.into()],
            )
            .unwrap();
        t.into_iter().next().map(|r| {
            (
                r.get::<String>(1).unwrap().unwrap(),
                r.get::<String>(2).unwrap().unwrap(),
            )
        })
    });
    match found {
        Some((declared, typcategory)) => ColumnMeta {
            declared,
            typcategory,
        },
        None => error!("postvec: filter: column {col:?} does not exist on the searched table"),
    }
}

/// Assert PostgreSQL's own parser can resolve the exact expression shape the
/// rendered predicate will use: same operator spelling, same declared type on
/// both sides (`::text` on the LIKE/ILIKE right side), same (caller's)
/// search_path — with NULL operands, so nothing evaluates against data. This
/// is real operator resolution, not a `pg_operator` approximation: it accepts
/// polymorphic operators (enum equality, array comparisons) and rejects
/// operators an *unqualified* expression could not actually reach. Valid type
/// *input* does not imply operator support, and this check runs before query
/// embedding so a missing operator never becomes a late "search query failed"
/// SPI error. Failure re-raises as a clean refusal (the transaction is
/// aborting either way, so no subtransaction is needed).
fn assert_operator(col: &str, meta: &ColumnMeta, op_sql: &str, rhs_is_text: bool) {
    let t = &meta.declared;
    let rhs = if rhs_is_text { "text" } else { t.as_str() };
    let probe = format!("SELECT (NULL::{t} {op_sql} NULL::{rhs}) IS NULL");
    let col = col.to_string();
    let declared = meta.declared.clone();
    let spelling = op_sql.to_string();
    pgrx::PgTryBuilder::new(|| {
        Spi::get_one::<bool>(&probe).ok();
    })
    .catch_others(move |_| {
        error!(
            "postvec: filter: column {col:?} ({declared}) has no {spelling} operator \
             PostgreSQL can resolve"
        )
    })
    .execute();
}

/// Validate a scalar as input for the column's exact declared type
/// (`format_type` with typmod, so length/precision limits and domain
/// constraints apply — the same conversion the rendered `::cast` performs).
fn assert_valid_input(col: &str, meta: &ColumnMeta, val: &ScalarVal) {
    let ok = Spi::get_one_with_args::<bool>(
        "SELECT pg_catalog.pg_input_is_valid($1, $2)",
        &[val.text.as_str().into(), meta.declared.as_str().into()],
    )
    .unwrap()
    .unwrap_or(false);
    if !ok {
        error!(
            "postvec: filter: {v:?} is not valid input for column {col:?} ({t})",
            v = val.text,
            t = meta.declared,
        );
    }
}

/// The rendered predicate (absent for a NULL/empty filter) plus its bind
/// values, in rendering order, to be appended after the search query's
/// `$1`/`$2` parameters.
pub(crate) struct RenderedFilter {
    pub(crate) predicate: Option<String>,
    pub(crate) values: Vec<String>,
}

impl RenderedFilter {
    pub(crate) fn none() -> Self {
        RenderedFilter {
            predicate: None,
            values: Vec::new(),
        }
    }
}

/// Parse, catalog-validate, and render a filter into one deterministic
/// AND-composed predicate over `alias`, with values as numbered bind
/// parameters starting at `first_param` and explicit casts to each column's
/// catalog type. Not pure: it deliberately consults PostgreSQL's catalog and
/// input functions. Every refusal happens here — before the caller pays for
/// a query embedding.
pub(crate) fn render_filter(
    rel_oid: pg_sys::Oid,
    alias: &str,
    filter: Option<&serde_json::Value>,
    first_param: usize,
) -> RenderedFilter {
    let Some(filter) = filter else {
        return RenderedFilter::none();
    };
    let serialized = serde_json::to_string(filter).unwrap_or_default();
    if serialized.len() > FILTER_MAX_BYTES {
        error!(
            "postvec: filter: serialized filter is {} bytes (max {FILTER_MAX_BYTES})",
            serialized.len()
        );
    }
    let parsed = parse_filter(filter);
    if parsed.is_empty() {
        return RenderedFilter::none();
    }

    let mut terms: Vec<String> = Vec::new();
    let mut values: Vec<String> = Vec::new();
    let mut param = first_param;
    for (col, conds) in parsed {
        let meta = column_meta(rel_oid, &col);
        let colref = format!("{alias}.{}", quote_ident(&col));
        let cast = &meta.declared;
        for cond in conds {
            match cond {
                Cond::Eq(v) => {
                    assert_operator(&col, &meta, "=", false);
                    assert_valid_input(&col, &meta, &v);
                    terms.push(format!("{colref} = ${param}::{cast}"));
                    values.push(v.text);
                    param += 1;
                }
                Cond::IsNull => terms.push(format!("{colref} IS NULL")),
                Cond::IsNotNull => terms.push(format!("{colref} IS NOT NULL")),
                Cond::Cmp { op, val } => {
                    assert_operator(&col, &meta, op, false);
                    assert_valid_input(&col, &meta, &val);
                    terms.push(format!("{colref} {op} ${param}::{cast}"));
                    values.push(val.text);
                    param += 1;
                }
                Cond::In(vals) => {
                    assert_operator(&col, &meta, "=", false);
                    let mut params = Vec::with_capacity(vals.len());
                    for v in vals {
                        assert_valid_input(&col, &meta, &v);
                        params.push(format!("${param}::{cast}"));
                        values.push(v.text);
                        param += 1;
                    }
                    terms.push(format!("{colref} IN ({})", params.join(", ")));
                }
                Cond::Like { insensitive, val } => {
                    if meta.typcategory != "S" {
                        error!(
                            "postvec: filter: {op} on column {col:?} requires a string-category \
                             type, not {t}",
                            op = if insensitive { "\"ilike\"" } else { "\"like\"" },
                            t = meta.declared,
                        );
                    }
                    let _ = val.is_string; // shape-checked at parse time
                    let kw = if insensitive { "ILIKE" } else { "LIKE" };
                    assert_operator(&col, &meta, kw, true);
                    // The pattern is validated against the exact declared
                    // type like every other value (typmod/domain rules
                    // apply), but binds as text: a varchar(n) cast would
                    // silently truncate long patterns at execution.
                    assert_valid_input(&col, &meta, &val);
                    terms.push(format!("{colref} {kw} ${param}::text"));
                    values.push(val.text);
                    param += 1;
                }
            }
        }
    }
    RenderedFilter {
        predicate: Some(terms.join(" AND ")),
        values,
    }
}

/// The semantic leg's match expressions: `(column side, query-parameter
/// side)`. Plain `col <op> $1::vector` up to pgvector's 2000-dim HNSW limit;
/// above it the only buildable ANN index is the halfvec expression form that
/// `migration_status()` / `create_vector_index()` suggest
/// (`hnsw ((col::halfvec(N)) halfvec_*_ops)`), and Postgres only uses an
/// expression index when the ORDER BY expression matches it — so the query
/// is rendered in exactly that form. An alias prefix on the column side does
/// not disturb expression-index matching (the parser resolves the alias back
/// to the column).
pub(crate) fn semantic_match_exprs(entry: &RegistryEntry, alias: &str) -> (String, String) {
    let prefix = if alias.is_empty() {
        String::new()
    } else {
        format!("{alias}.")
    };
    let vec = format!("{prefix}{}", quote_ident(&entry.vector_column));
    if entry.dim > 2000 {
        let dim = entry.dim;
        (
            format!("({vec}::halfvec({dim}))"),
            format!("$1::halfvec({dim})"),
        )
    } else {
        (vec, "$1::vector".to_string())
    }
}

/// The fixed internal alias the searched table carries in both candidate
/// CTEs — also the alias [`render_filter`] renders its predicate against.
pub(crate) const SEARCH_ALIAS: &str = "d";

/// Build and run the hybrid (or, when `qvec` is `None`, FTS-only) query.
/// The same filter predicate sits inside both candidate CTEs, before their
/// ORDER BY and LIMIT — the degraded FTS-only branch uses the same filtered
/// lexical CTE. Filter values bind after `$1` (query vector) / `$2` (query
/// text), in the renderer's deterministic order.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hybrid_rows(
    entry: &RegistryEntry,
    qvec: Option<&[f32]>,
    query: &str,
    limit_n: i32,
    weight: f32,
    rrf_k: i32,
    candidates: i32,
    filter: &RenderedFilter,
) -> Vec<Row> {
    if entry.is_recursive() {
        return chunk_hybrid_rows(
            entry, qvec, query, limit_n, weight, rrf_k, candidates, filter,
        );
    }
    let tbl = entry.qualified_table();
    let d = SEARCH_ALIAS;
    let pk = entry.pk_text_expr(d);
    let col = format!("{d}.{}", quote_ident(&entry.source_column));
    let vec = format!("{d}.{}", quote_ident(&entry.vector_column));
    let (sem, qparam) = semantic_match_exprs(entry, d);
    let op = crate::registry::distance_op(&entry.distance);
    let cfg = quote_literal(&entry.fts_config);
    let w = f64::from(weight);
    let k = rrf_k.max(1);
    let fpred = filter
        .predicate
        .as_deref()
        .map(|p| format!("\n                AND ({p})"))
        .unwrap_or_default();

    let fts_cte = format!(
        "fts AS (
             SELECT {pk} AS pk,
                    row_number() OVER (
                        ORDER BY ts_rank_cd(to_tsvector({cfg}::regconfig, {col}::text),
                                            websearch_to_tsquery({cfg}::regconfig, $2)) DESC
                    ) AS rank_fts
               FROM {tbl} {d}
              WHERE to_tsvector({cfg}::regconfig, {col}::text)
                    @@ websearch_to_tsquery({cfg}::regconfig, $2){fpred}
              ORDER BY ts_rank_cd(to_tsvector({cfg}::regconfig, {col}::text),
                                  websearch_to_tsquery({cfg}::regconfig, $2)) DESC
              LIMIT {candidates}
         )"
    );

    let sql = if qvec.is_some() {
        format!(
            "WITH semantic AS (
                 SELECT {pk} AS pk,
                        row_number() OVER (ORDER BY {sem} {op} {qparam}) AS rank_sem
                   FROM {tbl} {d}
                  WHERE {vec} IS NOT NULL{fpred}
                  ORDER BY {sem} {op} {qparam}
                  LIMIT {candidates}
             ),
             {fts_cte},
             fused AS (
                 SELECT COALESCE(s.pk, f.pk) AS pk,
                        COALESCE({w}::float8 / ({k} + s.rank_sem), 0)
                      + COALESCE((1 - {w}::float8) / ({k} + f.rank_fts), 0) AS rrf_score,
                        s.rank_sem, f.rank_fts
                   FROM semantic s FULL OUTER JOIN fts f USING (pk)
             )
             SELECT pk, rrf_score, rank_sem, rank_fts
               FROM fused ORDER BY rrf_score DESC NULLS LAST LIMIT {limit_n}"
        )
    } else {
        // Degraded: FTS only. rrf_score is 1/(k+rank_fts); rank_sem NULL.
        format!(
            "WITH {fts_cte},
             fused AS (
                 SELECT pk, (1.0 / ({k} + rank_fts))::float8 AS rrf_score,
                        NULL::bigint AS rank_sem, rank_fts
                   FROM fts
             )
             SELECT pk, rrf_score, rank_sem, rank_fts
               FROM fused ORDER BY rrf_score DESC LIMIT {limit_n}"
        )
    };

    let qvec_text = qvec.map(serialize_vector).unwrap_or_default();
    Spi::connect(|c| {
        let mut args: Vec<pgrx::datum::DatumWithOid> =
            vec![qvec_text.as_str().into(), query.into()];
        for v in &filter.values {
            args.push(v.as_str().into());
        }
        let table = c
            .select(sql.as_str(), None, &args)
            .expect("postvec: search query failed");
        table
            .into_iter()
            .map(|r| {
                (
                    r.get::<String>(1).unwrap().unwrap(),
                    r.get::<f64>(2).unwrap().unwrap_or(0.0),
                    r.get::<i64>(3).unwrap(),
                    r.get::<i64>(4).unwrap(),
                    None,
                    None,
                    None,
                    None,
                )
            })
            .collect()
    })
}

/// Recursive search shape: an explicit source join inside both candidate
/// CTEs. The planner keeps the HNSW scan with a nested-loop source join
/// (unfiltered / moderately selective filters) or inverts to a
/// filter-first exact ranking under highly selective filters. Both apply
/// source visibility and the metadata predicate before the candidate
/// LIMIT. Chunks fuse by their never-reused identity; one row per
/// document survives. The document's score is the maximum fused score of
/// any one chunk, so long documents do not win merely by having more
/// chunks. The winning chunk is deterministic: score, then semantic
/// rank, lexical rank, chunk_seq, chunk id.
#[allow(clippy::too_many_arguments)]
fn chunk_hybrid_rows(
    entry: &RegistryEntry,
    qvec: Option<&[f32]>,
    query: &str,
    limit_n: i32,
    weight: f32,
    rrf_k: i32,
    candidates: i32,
    filter: &RenderedFilter,
) -> Vec<Row> {
    let qdest = entry.qualified_vector_table();
    let qsrc = entry.qualified_table();
    let d = SEARCH_ALIAS;
    let src_join = entry.source_pk_join(d, "c");
    let vec = format!("c.{}", quote_ident(&entry.vector_column));
    let (sem, qparam) = semantic_match_exprs(entry, "c");
    let op = crate::registry::distance_op(&entry.distance);
    let cfg = quote_literal(&entry.fts_config);
    let w = f64::from(weight);
    let k = rrf_k.max(1);
    let fpred = filter
        .predicate
        .as_deref()
        .map(|p| format!("\n                AND ({p})"))
        .unwrap_or_default();

    // Deliberately NO chunk_text here: projecting it through the candidate
    // and fusion CTEs would materialize candidate-pool × chunk-size bytes in
    // the SPI result before any Rust-side ceiling could run. The winners'
    // text is fetched in a second, byte-budgeted phase below.
    let chunk_fields = "c.postvec_chunk_id AS cid, c.postvec_source_pk::text AS pk,
                    c.postvec_chunk_seq AS seq, c.postvec_char_start AS cs,
                    c.postvec_char_end AS ce";
    let fts_cte = format!(
        "fts_chunks AS (
             SELECT {chunk_fields},
                    row_number() OVER (
                        ORDER BY ts_rank_cd(to_tsvector({cfg}::regconfig, c.chunk_text),
                                            websearch_to_tsquery({cfg}::regconfig, $2)) DESC
                    ) AS rank_fts
               FROM {qdest} c JOIN {qsrc} {d} ON {src_join}
              WHERE to_tsvector({cfg}::regconfig, c.chunk_text)
                    @@ websearch_to_tsquery({cfg}::regconfig, $2){fpred}
              ORDER BY ts_rank_cd(to_tsvector({cfg}::regconfig, c.chunk_text),
                                  websearch_to_tsquery({cfg}::regconfig, $2)) DESC
              LIMIT {candidates}
         )"
    );
    // The deterministic winning-chunk order inside each document.
    let doc_window = "row_number() OVER (
                        PARTITION BY pk
                        ORDER BY rrf_score DESC,
                                 COALESCE(rank_sem, 9223372036854775807),
                                 COALESCE(rank_fts, 9223372036854775807),
                                 seq, cid) AS doc_row";

    let sql = if qvec.is_some() {
        format!(
            "WITH semantic_chunks AS (
                 SELECT {chunk_fields},
                        row_number() OVER (ORDER BY {sem} {op} {qparam}) AS rank_sem
                   FROM {qdest} c JOIN {qsrc} {d} ON {src_join}
                  WHERE {vec} IS NOT NULL{fpred}
                  ORDER BY {sem} {op} {qparam}
                  LIMIT {candidates}
             ),
             {fts_cte},
             fused_chunks AS (
                 SELECT COALESCE(s.cid, f.cid) AS cid,
                        COALESCE(s.pk, f.pk) AS pk,
                        COALESCE(s.seq, f.seq) AS seq,
                        COALESCE(s.cs, f.cs) AS cs,
                        COALESCE(s.ce, f.ce) AS ce,
                        COALESCE({w}::float8 / ({k} + s.rank_sem), 0)
                      + COALESCE((1 - {w}::float8) / ({k} + f.rank_fts), 0) AS rrf_score,
                        s.rank_sem, f.rank_fts
                   FROM semantic_chunks s FULL OUTER JOIN fts_chunks f USING (cid)
             ),
             best_per_document AS (
                 SELECT *, {doc_window} FROM fused_chunks
             )
             SELECT cid, pk, rrf_score, rank_sem, rank_fts, seq, cs, ce
               FROM best_per_document
              WHERE doc_row = 1
              ORDER BY rrf_score DESC NULLS LAST LIMIT {limit_n}"
        )
    } else {
        // Degraded: lexical chunks only, same document collapse.
        format!(
            "WITH {fts_cte},
             fused_chunks AS (
                 SELECT cid, pk, seq, cs, ce,
                        (1.0 / ({k} + rank_fts))::float8 AS rrf_score,
                        NULL::bigint AS rank_sem, rank_fts
                   FROM fts_chunks
             ),
             best_per_document AS (
                 SELECT *, {doc_window} FROM fused_chunks
             )
             SELECT cid, pk, rrf_score, rank_sem, rank_fts, seq, cs, ce
               FROM best_per_document
              WHERE doc_row = 1
              ORDER BY rrf_score DESC LIMIT {limit_n}"
        )
    };

    let qvec_text = qvec.map(serialize_vector).unwrap_or_default();
    Spi::connect(|c| {
        let mut args: Vec<pgrx::datum::DatumWithOid> =
            vec![qvec_text.as_str().into(), query.into()];
        for v in &filter.values {
            args.push(v.as_str().into());
        }
        let table = c
            .select(sql.as_str(), None, &args)
            .expect("postvec: chunk search query failed");
        let mut cids: Vec<i64> = Vec::new();
        let mut rows: Vec<Row> = Vec::new();
        for r in table {
            cids.push(r.get::<i64>(1).unwrap().unwrap());
            rows.push((
                r.get::<String>(2).unwrap().unwrap(),
                r.get::<f64>(3).unwrap().unwrap_or(0.0),
                r.get::<i64>(4).unwrap(),
                r.get::<i64>(5).unwrap(),
                r.get::<i32>(6).unwrap(),
                r.get::<i64>(7).unwrap(),
                r.get::<i64>(8).unwrap(),
                None,
            ));
        }

        // Winner-text phases (round 4): (2a) fetch LENGTHS only for the
        // ≤ limit_n winning chunks; (2b) admit ids in result order under the
        // byte ceiling, in Rust, before any text exists anywhere; (2c) fetch
        // text for the admitted ids only — the database never materializes
        // (and Rust never copies) more than the budget of result text. The
        // earlier window-query form carried chunk_text through a sort, so
        // PostgreSQL could still materialize every winner's text before the
        // outer CASE stripped it. Winners past the ceiling keep their row
        // with chunk_text NULL, under one WARNING. Strings MOVE from the
        // fetch map into the rows — no second copy.
        if !cids.is_empty() {
            let lens: Vec<(i64, i64)> = {
                let t = c
                    .select(
                        &format!(
                            "SELECT u.cid, octet_length(c.chunk_text)::bigint
                               FROM unnest($1::int8[]) WITH ORDINALITY AS u(cid, ord)
                               JOIN {qdest} c ON c.postvec_chunk_id = u.cid
                              ORDER BY u.ord"
                        ),
                        None,
                        &[cids.clone().into()],
                    )
                    .expect("postvec: chunk text length fetch failed");
                t.into_iter()
                    .map(|r| {
                        (
                            r.get::<i64>(1).unwrap().unwrap(),
                            r.get::<i64>(2).unwrap().unwrap_or(0),
                        )
                    })
                    .collect()
            };
            let mut admitted: Vec<i64> = Vec::with_capacity(lens.len());
            let mut admitted_lens: Vec<i64> = Vec::with_capacity(lens.len());
            let mut total = 0i64;
            let mut truncated = false;
            for (cid, len) in lens {
                if total.saturating_add(len) <= RESULT_TEXT_MAX_BYTES as i64 {
                    total += len;
                    admitted.push(cid);
                    admitted_lens.push(len);
                } else {
                    truncated = true;
                }
            }
            if truncated {
                warning!(
                    "postvec: search result chunk text exceeds {RESULT_TEXT_MAX_BYTES} \
                     summed bytes; later rows return chunk_text NULL — lower limit_n or \
                     re-create the entry with a smaller chunk_size"
                );
            }
            if !admitted.is_empty() {
                // Per-row admitted-length recheck (chunk rows are written
                // once, but a replaced chunk id could in principle carry
                // different text; the recheck makes the ceiling
                // unconditional).
                let mut texts: std::collections::BTreeMap<i64, String> = {
                    let t = c
                        .select(
                            &format!(
                                "SELECT c.postvec_chunk_id,
                                        CASE WHEN octet_length(c.chunk_text)::bigint <= u.len
                                             THEN c.chunk_text END
                                   FROM unnest($1::int8[], $2::int8[]) AS u(cid, len)
                                   JOIN {qdest} c ON c.postvec_chunk_id = u.cid"
                            ),
                            None,
                            &[admitted.into(), admitted_lens.into()],
                        )
                        .expect("postvec: chunk text fetch failed");
                    t.into_iter()
                        .filter_map(|r| {
                            let cid = r.get::<i64>(1).unwrap().unwrap();
                            r.get::<String>(2).unwrap().map(|txt| (cid, txt))
                        })
                        .collect()
                };
                for (row, cid) in rows.iter_mut().zip(cids.iter()) {
                    // A chunk deleted between phases simply keeps NULL.
                    row.7 = texts.remove(cid);
                }
            }
        }
        rows
    })
}

/// Read-only guard for observed entries. With no DML triggers, a
/// dropped-and-recreated same-named table would otherwise be queried as if
/// it were the adopted one. The exact TRUNCATE sentinel is the
/// relation-identity check; synced entries pay no extra catalog query.
fn assert_observed_sentinel(entry: &RegistryEntry) {
    if entry.trigger_mode == "none" && entry.triggers_missing() {
        error!(
            "postvec: {}.{}.{} is an observed entry whose TRUNCATE sentinel is gone \
             (table recreated?); re-run postvec.adopt() on the new table",
            entry.table_schema, entry.table_name, entry.source_column
        );
    }
}

/// Upper bounds on user-controlled result and candidate sizes: hybrid search
/// materializes its rows (including chunk text) in backend memory before
/// returning them, so `limit_n`/`candidates` are a memory and CPU lever any
/// SQL caller holds. The ceilings are far above useful retrieval sizes.
const MAX_SEARCH_LIMIT: i32 = 1_000;
const MAX_SEARCH_CANDIDATES: i32 = 100_000;

fn validate_search_args(limit_n: i32, semantic_weight: f32, rrf_k: i32, candidates: i32) {
    if !(1..=MAX_SEARCH_LIMIT).contains(&limit_n) {
        error!("postvec: limit_n must be between 1 and {MAX_SEARCH_LIMIT}");
    }
    if !(1..=MAX_SEARCH_CANDIDATES).contains(&candidates) {
        error!("postvec: candidates must be between 1 and {MAX_SEARCH_CANDIDATES}");
    }
    if rrf_k < 1 {
        error!("postvec: rrf_k must be >= 1");
    }
    if !semantic_weight.is_finite() || !(0.0..=1.0).contains(&semantic_weight) {
        error!("postvec: semantic_weight must be finite and between 0 and 1");
    }
}

#[pg_extern]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn search(
    relation: &str,
    column_name: &str,
    query: &str,
    limit_n: default!(i32, 10),
    semantic_weight: default!(f32, 0.5),
    rrf_k: default!(i32, 60),
    candidates: default!(Option<i32>, "NULL"),
    filter: default!(Option<JsonB>, "NULL"),
) -> TableIterator<
    'static,
    (
        name!(pk_value, String),
        name!(rrf_score, f64),
        name!(semantic_rank, Option<i64>),
        name!(fts_rank, Option<i64>),
        name!(chunk_seq, Option<i32>),
        name!(chunk_start, Option<i64>),
        name!(chunk_end, Option<i64>),
        name!(chunk_text, Option<String>),
    ),
> {
    let rel = search_relation(relation);
    let (schema, table) = (rel.schema.clone(), rel.table.clone());
    let entry = RegistryEntry::load_active(&schema, &table, column_name)
        .unwrap_or_else(|| error!("postvec: {schema}.{table}.{column_name} is not enabled"));
    assert_observed_sentinel(&entry);
    crate::api::embed::assert_input_within_cap("search query", query.len());

    // Chunk-level candidates need more headroom: one document can occupy
    // many candidate slots before the collapse. Saturating: the raw
    // arguments are validated below, after the default is derived.
    let cand = candidates.unwrap_or_else(|| {
        if entry.is_recursive() {
            limit_n.saturating_mul(16).max(200)
        } else {
            limit_n.saturating_mul(4).max(50)
        }
    });
    validate_search_args(limit_n, semantic_weight, rrf_k, cand);

    // Resolve and validate the filter BEFORE the synchronous query-embedding
    // call: malformed filters must never cost an inference request.
    let rendered = render_filter(rel.oid, SEARCH_ALIAS, filter.as_ref().map(|j| &j.0), 3);

    // Embed the query (the one inline network call), or degrade to FTS-only.
    // The response is validated before it reaches the dynamic SQL: a wrong
    // cardinality, wrong dimension, or non-finite component would otherwise
    // surface as a late pgvector/SPI error instead of a clear postvec one.
    let qvec = match embed_texts(&[query.to_string()], &entry.model) {
        Ok(vecs) => match validate_query_embedding(&entry, vecs) {
            Ok(v) => Some(v),
            Err(reason) => degrade_or_error(&reason),
        },
        Err(e) => degrade_or_error(&format!("{e}")),
    };

    let rows = hybrid_rows(
        &entry,
        qvec.as_deref(),
        query,
        limit_n,
        semantic_weight,
        rrf_k,
        cand,
        &rendered,
    );
    TableIterator::new(rows)
}

/// Like [`search`] but with a caller-supplied query vector — skips the inline
/// embed call. Use when you already have the query embedding (app-side caching)
/// or to search without a reachable ninference. `query_text` (may be empty)
/// still drives the FTS leg. `filter` has identical semantics to `search()`'s.
#[pg_extern]
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn search_with_vector<'a>(
    relation: &str,
    column_name: &str,
    query_vector: pgrx::Array<'a, f32>,
    query_text: default!(&str, "''"),
    limit_n: default!(i32, 10),
    semantic_weight: default!(f32, 0.5),
    rrf_k: default!(i32, 60),
    candidates: default!(Option<i32>, "NULL"),
    filter: default!(Option<JsonB>, "NULL"),
) -> TableIterator<
    'static,
    (
        name!(pk_value, String),
        name!(rrf_score, f64),
        name!(semantic_rank, Option<i64>),
        name!(fts_rank, Option<i64>),
        name!(chunk_seq, Option<i32>),
        name!(chunk_start, Option<i64>),
        name!(chunk_end, Option<i64>),
        name!(chunk_text, Option<String>),
    ),
> {
    let rel = search_relation(relation);
    let (schema, table) = (rel.schema.clone(), rel.table.clone());
    let entry = RegistryEntry::load_active(&schema, &table, column_name)
        .unwrap_or_else(|| error!("postvec: {schema}.{table}.{column_name} is not enabled"));
    assert_observed_sentinel(&entry);
    // Dimension checked against the borrowed array datum before any Rust
    // copy; the copy below is bounded by the entry's declared dimension.
    if query_vector.len() as i32 != entry.dim {
        error!(
            "postvec: query vector has {} dims but {schema}.{table}.{column_name} is vector({})",
            query_vector.len(),
            entry.dim
        );
    }
    let query_vector: Vec<f32> = query_vector
        .iter()
        .map(|v| v.unwrap_or_else(|| error!("postvec: query vector contains a NULL element")))
        .collect();
    if !query_vector.iter().all(|f| f.is_finite()) {
        error!("postvec: query vector contains a non-finite component (NaN/Inf)");
    }
    crate::api::embed::assert_input_within_cap("query_text", query_text.len());
    // Chunk-level candidates need more headroom: one document can occupy
    // many candidate slots before the collapse. Saturating: the raw
    // arguments are validated below, after the default is derived.
    let cand = candidates.unwrap_or_else(|| {
        if entry.is_recursive() {
            limit_n.saturating_mul(16).max(200)
        } else {
            limit_n.saturating_mul(4).max(50)
        }
    });
    validate_search_args(limit_n, semantic_weight, rrf_k, cand);
    let rendered = render_filter(rel.oid, SEARCH_ALIAS, filter.as_ref().map(|j| &j.0), 3);
    let rows = hybrid_rows(
        &entry,
        Some(&query_vector),
        query_text,
        limit_n,
        semantic_weight,
        rrf_k,
        cand,
        &rendered,
    );
    TableIterator::new(rows)
}

// Selective filters can exhaust HNSW's default fixed candidate scan before
// finding `candidates` matching rows. pgvector 0.8's iterative scans fix
// exactly that; `relaxed_order` is appropriate because postvec consumes
// ordinal ranks for RRF, not raw distance order as a public result contract.
// This is a function-local GUC on the exact installed signatures (a
// non-search-path setting, so pgrx's `#[search_path]` mechanism does not
// apply — it needs exact-signature SQL).
extension_sql!(
    r#"
ALTER FUNCTION postvec.search(text, text, text, integer, real, integer, integer, jsonb)
    SET hnsw.iterative_scan = 'relaxed_order';
ALTER FUNCTION postvec.search_with_vector(text, text, real[], text, integer, real, integer, integer, jsonb)
    SET hnsw.iterative_scan = 'relaxed_order';
"#,
    name = "postvec_search_iterative_scan",
    requires = [search, search_with_vector]
);

/// Validate the model's response to a single-query embed request before it is
/// rendered into SQL: exactly one vector, matching the column's dimension,
/// all components finite. Mirrors the checks `search_with_vector` applies to
/// caller-supplied vectors.
pub(crate) fn validate_query_embedding(
    entry: &RegistryEntry,
    mut vecs: Vec<Vec<f32>>,
) -> Result<Vec<f32>, String> {
    if vecs.len() != 1 {
        return Err(format!(
            "ninference returned {} query embeddings, expected exactly 1",
            vecs.len()
        ));
    }
    let v = vecs.swap_remove(0);
    if v.len() as i32 != entry.dim {
        return Err(format!(
            "query embedding has {} dims but the column is vector({})",
            v.len(),
            entry.dim
        ));
    }
    if !v.iter().all(|f| f.is_finite()) {
        return Err("query embedding contains a non-finite component (NaN/Inf)".into());
    }
    Ok(v)
}

/// Return `None` (FTS-only) with a WARNING when degradation is enabled, else
/// raise the error.
fn degrade_or_error(reason: &str) -> Option<Vec<f32>> {
    if gucs::SEARCH_DEGRADE_TO_FTS.get() {
        warning!("postvec: search degraded to FTS-only ({reason})");
        None
    } else {
        error!("postvec: query embedding failed and degradation is off: {reason}");
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use crate::registry::{serialize_vector, RegistryEntry};
    use pgrx::prelude::*;

    fn setup() -> RegistryEntry {
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO docs (body) VALUES
                ('quarterly revenue guidance was raised'),
                ('the office plants need watering'),
                ('vector conversion retains high MRR')",
        )
        .unwrap();
        // backfill => false: we fill vectors by hand for a deterministic KNN.
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();

        // Hand-place vectors: row 1 closest to [1,0,0], row 3 next, row 2 far.
        for (id, v) in [
            (1i64, [1.0f32, 0.0, 0.0]),
            (2, [0.0, 0.0, 1.0]),
            (3, [0.9, 0.1, 0.0]),
        ] {
            let vtext = serialize_vector(&v);
            Spi::run_with_args(
                "UPDATE docs SET body_semantic = $1::vector WHERE id = $2",
                &[vtext.as_str().into(), id.into()],
            )
            .unwrap();
        }
        RegistryEntry::load_active("public", "docs", "body").unwrap()
    }

    #[pg_test]
    fn hybrid_ranks_semantic_neighbour_first() {
        let entry = setup();
        // Query vector nearest row 1, then row 3.
        let q = [1.0f32, 0.0, 0.0];
        let rows = super::hybrid_rows(
            &entry,
            Some(&q),
            "nothing matches keywords",
            10,
            0.5,
            60,
            50,
            &super::RenderedFilter::none(),
        );
        assert!(!rows.is_empty());
        assert_eq!(rows[0].0, "1", "closest vector ranks first");
    }

    /// FTS stays anchored on the raw source column. Words that exist only
    /// in template context columns are invisible to the lexical leg.
    #[pg_test]
    fn fts_ignores_template_only_words() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE fdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                 title text, body text)",
        )
        .unwrap();
        Spi::run("INSERT INTO fdocs (title, body) VALUES ('zebrastripe', 'plain words')").unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('fdocs','body','m', backfill => false,
                                   format => '$title $body')",
        )
        .unwrap();
        let entry = RegistryEntry::load_active("public", "fdocs", "body").unwrap();
        let rows = super::hybrid_rows(
            &entry,
            None,
            "zebrastripe",
            10,
            0.5,
            60,
            50,
            &super::RenderedFilter::none(),
        );
        assert!(rows.is_empty(), "a template-only word has no FTS hit");
        let rows = super::hybrid_rows(
            &entry,
            None,
            "plain",
            10,
            0.5,
            60,
            50,
            &super::RenderedFilter::none(),
        );
        assert_eq!(rows.len(), 1, "source-column words still hit");
    }

    #[pg_test]
    fn fts_only_degrade_still_finds_keyword() {
        let entry = setup();
        // qvec None => FTS-only path.
        let rows = super::hybrid_rows(
            &entry,
            None,
            "revenue guidance",
            10,
            0.5,
            60,
            50,
            &super::RenderedFilter::none(),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "1", "keyword hit is the revenue-guidance row");
        assert!(rows[0].2.is_none(), "no semantic rank in degraded mode");
    }

    /// `search()` on a column enabled on a convert-only model (embed-bridge
    /// routed): query-embed resolution succeeds via the bridge; with no
    /// endpoints configured the network call fails and search degrades to
    /// FTS — it must not error out at model resolution.
    #[pg_test]
    fn search_on_bridged_entry_degrades_to_fts() {
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
        Spi::run("INSERT INTO bdocs (body) VALUES ('quarterly revenue guidance was raised')")
            .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('bdocs','body','ext', backfill => false)")
            .unwrap();
        Spi::run("UPDATE bdocs SET body_semantic = '[1,0,0,0]'::vector").unwrap();

        let pk = Spi::get_one::<String>(
            "SELECT pk_value FROM postvec.search('bdocs','body','revenue guidance') LIMIT 1",
        )
        .unwrap();
        assert_eq!(pk.as_deref(), Some("1"), "FTS leg still finds the row");
    }

    #[pg_test]
    fn search_with_vector_skips_embed_and_ranks() {
        setup();
        // Caller-supplied query vector nearest row 1; no ninference involved.
        let pk = Spi::get_one::<String>(
            "SELECT pk_value FROM postvec.search_with_vector(
                 'docs','body', ARRAY[1,0,0]::real[], 'no keywords') LIMIT 1",
        )
        .unwrap();
        assert_eq!(pk.as_deref(), Some("1"), "closest vector ranks first");
    }

    #[pg_test]
    fn search_with_vector_rejects_wrong_dim() {
        setup();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<String>(
                "SELECT pk_value FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0]::real[]) LIMIT 1",
            )
            .ok();
        });
        assert!(r.is_err(), "a mismatched query-vector dimension must error");
    }

    #[pg_test]
    fn search_with_vector_rejects_non_finite_vector() {
        setup();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<String>(
                "SELECT pk_value FROM postvec.search_with_vector(
                     'docs','body', ARRAY['NaN','0','0']::real[]) LIMIT 1",
            )
            .ok();
        });
        assert!(
            r.is_err(),
            "a NaN query-vector component must error up front"
        );
    }

    /// Above pgvector's 2000-dim HNSW limit the semantic leg renders in the
    /// halfvec expression form, so the recommended expression index
    /// (`hnsw ((col::halfvec(N)) halfvec_*_ops)`) actually accelerates
    /// search instead of being a finalization-only marker.
    #[pg_test]
    fn high_dim_search_uses_the_halfvec_expression_form() {
        const DIM: usize = 2100;
        Spi::run_with_args(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('hd','embed','hd',$1,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
            &[(DIM as i32).into()],
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE hdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        Spi::run("INSERT INTO hdocs (body) VALUES ('one'), ('two'), ('three')").unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('hdocs','body','hd', backfill => false)")
            .unwrap();

        // Build the suggested expression index BEFORE planting vectors: the
        // whole test runs in one transaction, and updating the column first
        // would leave broken HOT chains that flag the index `indcheckxmin`,
        // making it invisible to this very transaction (a harness artifact —
        // ordinary multi-transaction usage is unaffected).
        Spi::run(&format!(
            "CREATE INDEX hdocs_halfvec ON hdocs
              USING hnsw ((body_semantic::halfvec({DIM})) halfvec_cosine_ops)"
        ))
        .unwrap();

        let mut vecs = vec![vec![0.0f32; DIM]; 3];
        vecs[0][0] = 1.0; // row 1: exact match for the query below
        vecs[1][DIM - 1] = 1.0; // row 2: orthogonal
        vecs[2][0] = 0.9; // row 3: close second
        vecs[2][1] = 0.1;
        for (i, v) in vecs.iter().enumerate() {
            Spi::run_with_args(
                "UPDATE hdocs SET body_semantic = $1::vector WHERE id = $2",
                &[serialize_vector(v).as_str().into(), ((i + 1) as i64).into()],
            )
            .unwrap();
        }
        let entry = RegistryEntry::load_active("public", "hdocs", "body").unwrap();

        // The halfvec-rendered semantic leg executes and ranks correctly.
        let mut q = vec![0.0f32; DIM];
        q[0] = 1.0;
        let rows = super::hybrid_rows(
            &entry,
            Some(&q),
            "no keyword match",
            10,
            1.0,
            60,
            50,
            &super::RenderedFilter::none(),
        );
        assert_eq!(rows[0].0, "1", "nearest high-dim vector ranks first");

        // And the suggested expression index is actually usable by that leg.
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        let (sem, qparam) = super::semantic_match_exprs(&entry, "");
        let op = crate::registry::distance_op(&entry.distance);
        let plan: Vec<String> = Spi::connect(|c| {
            c.select(
                &format!(
                    "EXPLAIN SELECT id FROM hdocs WHERE body_semantic IS NOT NULL
                      ORDER BY {sem} {op} {qparam} LIMIT 2"
                ),
                None,
                &[serialize_vector(&q).as_str().into()],
            )
            .unwrap()
            .map(|r| r.get::<String>(1).unwrap().unwrap())
            .collect()
        });
        assert!(
            plan.iter().any(|l| l.contains("hdocs_halfvec")),
            "the halfvec expression index must serve the semantic leg: {plan:?}"
        );
    }

    #[pg_test]
    fn search_rejects_non_finite_weight() {
        setup();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<String>(
                "SELECT pk_value FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0,0]::real[], 'query',
                     semantic_weight => 'NaN'::real) LIMIT 1",
            )
            .ok();
        });
        assert!(
            r.is_err(),
            "NaN semantic_weight must error before SQL rendering"
        );
    }

    // ---- Typed metadata filters ----

    /// A metadata-rich fixture: three rows with hand-placed vectors (row 1
    /// nearest [1,0,0], row 3 second, row 2 far) and typed filter columns.
    fn setup_filtered() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run(
            r#"CREATE TABLE docs (
                   id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                   body text,
                   category text,
                   price numeric(8,2),
                   published_at timestamptz,
                   region text,
                   archived_at timestamptz,
                   title varchar(50),
                   flag boolean,
                   pt point,
                   "we""ird" text)"#,
        )
        .unwrap();
        Spi::run(
            "INSERT INTO docs
                 (body, category, price, published_at, region, archived_at, title, flag, \"we\"\"ird\")
             VALUES
                 ('quarterly revenue guidance was raised',
                  'finance', 100.00, '2026-01-15', 'EU', NULL, 'Q3 outlook', true, 'x'),
                 ('the office plants need watering',
                  'office', 5.00, '2025-06-01', 'US', '2026-01-01', 'plants', false, 'y'),
                 ('vector conversion retains high MRR',
                  'finance', 50.50, '2026-03-01', 'UK', NULL, 'Q3 report', false, 'z')",
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('docs','body','m', backfill => false)").unwrap();
        for (id, v) in [
            (1i64, [1.0f32, 0.0, 0.0]),
            (2, [0.0, 0.0, 1.0]),
            (3, [0.9, 0.1, 0.0]),
        ] {
            Spi::run_with_args(
                "UPDATE docs SET body_semantic = $1::vector WHERE id = $2",
                &[serialize_vector(&v).as_str().into(), id.into()],
            )
            .unwrap();
        }
    }

    /// pk_values (sorted) returned by search_with_vector with a filter.
    fn filtered_pks(filter: &str) -> Vec<String> {
        let q = format!(
            "SELECT array_agg(pk_value ORDER BY pk_value)
               FROM postvec.search_with_vector(
                    'docs','body', ARRAY[1,0,0]::real[], 'revenue guidance',
                    filter => {filter})"
        );
        Spi::get_one::<Vec<String>>(&q).unwrap().unwrap_or_default()
    }

    /// The filter excludes the nearest semantic row AND the strongest keyword
    /// row (row 1 is both); it must enter neither leg nor the fusion.
    #[pg_test]
    fn filter_narrows_both_legs() {
        setup_filtered();
        let pks = filtered_pks("'{\"category\": \"office\"}'::jsonb");
        assert_eq!(pks, vec!["2"], "only the office row survives the filter");

        // Sanity: without the filter row 1 leads both legs.
        let pks = filtered_pks("NULL::jsonb");
        assert!(pks.contains(&"1".to_string()));
    }

    #[pg_test]
    fn filter_null_and_empty_are_current_query() {
        setup_filtered();
        let unfiltered = filtered_pks("NULL::jsonb");
        assert_eq!(filtered_pks("'{}'::jsonb"), unfiltered);
        let no_param = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(pk_value ORDER BY pk_value)
               FROM postvec.search_with_vector(
                    'docs','body', ARRAY[1,0,0]::real[], 'revenue guidance')",
        )
        .unwrap()
        .unwrap();
        assert_eq!(no_param, unfiltered);
    }

    #[pg_test]
    fn filter_operator_matrix() {
        setup_filtered();
        for (filter, expected, why) in [
            (
                "'{\"category\": \"finance\"}'",
                vec!["1", "3"],
                "scalar equality",
            ),
            (
                "'{\"archived_at\": null}'",
                vec!["1", "3"],
                "JSON null = IS NULL",
            ),
            (
                "'{\"archived_at\": {\"is_not\": null}}'",
                vec!["2"],
                "is_not null = IS NOT NULL",
            ),
            (
                "'{\"price\": {\"neq\": 100, \"lte\": 60}}'",
                vec!["2", "3"],
                "two numeric operators AND-compose",
            ),
            (
                "'{\"published_at\": {\"gt\": \"2026-01-01\"}}'",
                vec!["1", "3"],
                "timestamptz comparison with cast",
            ),
            (
                "'{\"region\": [\"EU\", \"UK\"]}'",
                vec!["1", "3"],
                "array shorthand for IN",
            ),
            (
                "'{\"region\": {\"in\": [\"EU\", \"UK\"]}}'",
                vec!["1", "3"],
                "explicit in spelling",
            ),
            (
                "'{\"title\": {\"like\": \"Q3%\"}}'",
                vec!["1", "3"],
                "LIKE on varchar",
            ),
            (
                "'{\"title\": {\"ilike\": \"q3%\"}}'",
                vec!["1", "3"],
                "ILIKE on varchar",
            ),
            ("'{\"flag\": true}'", vec!["1"], "boolean equality"),
            (
                "'{\"price\": {\"gte\": 50, \"lt\": 100}}'",
                vec!["3"],
                "numeric range",
            ),
        ] {
            let pks = filtered_pks(&format!("{filter}::jsonb"));
            assert_eq!(pks, expected, "{why}: {filter}");
        }
    }

    #[pg_test]
    fn filter_shape_and_limit_refusals() {
        setup_filtered();
        let mut bad: Vec<(String, &str)> = vec![
            ("'[1, 2]'".into(), "top-level array"),
            ("'\"finance\"'".into(), "top-level scalar"),
            ("'{\"category\": {}}'".into(), "empty operator object"),
            (
                "'{\"category\": {\"in\": \"finance\"}}'".into(),
                "in with a non-array",
            ),
            ("'{\"category\": {\"in\": []}}'".into(), "empty in list"),
            (
                "'{\"category\": {\"in\": [\"a\", null]}}'".into(),
                "null inside in",
            ),
            (
                "'{\"category\": {\"in\": [[\"a\"]]}}'".into(),
                "nested array inside in",
            ),
            (
                "'{\"price\": {\"gt\": null}}'".into(),
                "null comparison value",
            ),
            (
                "'{\"title\": {\"like\": 5}}'".into(),
                "non-string like pattern",
            ),
            (
                "'{\"category\": {\"between\": [1, 2]}}'".into(),
                "unknown operator",
            ),
            (
                "'{\"category\": {\"is_not\": \"x\"}}'".into(),
                "is_not with a non-null",
            ),
            (
                "'{\"category\": {\"neq\": {\"a\": 1}}}'".into(),
                "nested object value",
            ),
            (
                "'{\"category\": [\"a\", {\"b\": 1}]}'".into(),
                "nested object inside the array shorthand",
            ),
        ];
        // The three query-shape caps: 33 columns, 257 in-values, > 64 KiB.
        let cols: Vec<String> = (0..33).map(|i| format!("\"c{i}\": 1")).collect();
        bad.push((format!("'{{{}}}'", cols.join(", ")), "33 columns"));
        let vals: Vec<String> = (0..257).map(|i| i.to_string()).collect();
        bad.push((
            format!("'{{\"price\": [{}]}}'", vals.join(", ")),
            "257 in values",
        ));
        bad.push((
            format!("'{{\"category\": \"{}\"}}'", "x".repeat(65 * 1024)),
            "over 64 KiB serialized",
        ));

        for (filter, why) in bad {
            let q = format!(
                "SELECT count(*) FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0,0]::real[], 'q', filter => {filter}::jsonb)"
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "{why} must refuse");
        }
    }

    /// Argument ceilings: oversized query text, out-of-range limit /
    /// candidates, and a degenerate rrf_k all refuse before any inference or
    /// candidate query runs.
    #[pg_test]
    fn search_refuses_oversized_query_and_out_of_range_args() {
        setup();
        let huge = "x".repeat(1_048_577); // postvec.max_document_bytes default + 1
        for (q, why) in [
            (
                format!(
                    "SELECT count(*) FROM postvec.search('docs','body','{}')",
                    huge
                ),
                "oversized query",
            ),
            (
                "SELECT count(*) FROM postvec.search('docs','body','q', limit_n => 100000)"
                    .to_string(),
                "limit over the ceiling",
            ),
            (
                "SELECT count(*) FROM postvec.search('docs','body','q', candidates => 1000000)"
                    .to_string(),
                "candidates over the ceiling",
            ),
            (
                "SELECT count(*) FROM postvec.search('docs','body','q', rrf_k => 0)".to_string(),
                "non-positive rrf_k",
            ),
        ] {
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "{why} must refuse");
        }
    }

    #[pg_test]
    fn filter_refuses_unknown_column_invalid_type_and_nontext_like() {
        setup_filtered();
        for (filter, why) in [
            ("'{\"ghost\": 1}'", "unknown column"),
            ("'{\"price\": \"abc\"}'", "invalid numeric input"),
            (
                "'{\"published_at\": {\"gt\": \"not a date\"}}'",
                "invalid timestamptz input",
            ),
            (
                "'{\"title\": \"a much too long value for a varchar(50) column padded padded padded\"}'",
                "typmod-invalid input",
            ),
            ("'{\"price\": {\"like\": \"5%\"}}'", "LIKE on numeric"),
            (
                "'{\"pt\": \"(1,1)\"}'",
                "equality on point, a type with valid input but no = operator",
            ),
        ] {
            let q = format!(
                "SELECT count(*) FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0,0]::real[], 'q', filter => {filter}::jsonb)"
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(r.is_err(), "{why} must refuse");
        }
        // A dropped column refuses like an unknown one.
        Spi::run("ALTER TABLE docs DROP COLUMN flag").unwrap();
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0,0]::real[], 'q',
                     filter => '{\"flag\": true}'::jsonb)",
            )
            .ok();
        });
        assert!(r.is_err(), "a dropped column must refuse");
    }

    /// SQL-looking values stay inert bound parameters — including
    /// backslash+quote input under `standard_conforming_strings = off`, the
    /// setting that makes naive apostrophe-doubling exploitable — and quoted
    /// identifiers round-trip.
    #[pg_test]
    fn filter_sql_looking_value_is_inert_and_identifiers_quote() {
        setup_filtered();
        Spi::run("SET LOCAL standard_conforming_strings = off").unwrap();
        for hostile in ["'; DROP TABLE docs; --", "\\'; DROP TABLE docs; --"] {
            let n = Spi::get_one_with_args::<i64>(
                "SELECT count(*) FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0,0]::real[], 'q', filter => $1)",
                &[pgrx::JsonB(serde_json::json!({ "category": hostile })).into()],
            )
            .unwrap();
            assert_eq!(n, Some(0), "the hostile value matches nothing");
            assert!(
                Spi::get_one::<bool>("SELECT to_regclass('docs') IS NOT NULL")
                    .unwrap()
                    .unwrap(),
                "docs survives: the value was data, not SQL"
            );
        }
        // Quoted identifier: the JSON key is the exact column name.
        let n = Spi::get_one_with_args::<i64>(
            "SELECT count(*) FROM postvec.search_with_vector(
                 'docs','body', ARRAY[1,0,0]::real[], 'q', filter => $1)",
            &[pgrx::JsonB(serde_json::json!({ "we\"ird": "x" })).into()],
        )
        .unwrap();
        assert_eq!(n, Some(1), "the quote-bearing column name resolves");
    }

    /// The degraded FTS-only path (no reachable inference) keeps the filter,
    /// and an invalid filter still errors — before any embed attempt, not
    /// masked by degradation.
    #[pg_test]
    fn filter_survives_fts_degradation() {
        setup_filtered();
        // No endpoints configured: search() degrades to FTS-only.
        let pks = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(pk_value)
               FROM postvec.search('docs','body','revenue guidance',
                                   filter => '{\"category\": \"office\"}'::jsonb)",
        )
        .unwrap();
        assert_eq!(
            pks, None,
            "the keyword row is filtered out of the degraded lexical leg"
        );
        let pks = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(pk_value)
               FROM postvec.search('docs','body','revenue guidance',
                                   filter => '{\"category\": \"finance\"}'::jsonb)",
        )
        .unwrap()
        .unwrap_or_default();
        assert_eq!(pks, vec!["1"], "the passing keyword row is returned");

        // Invalid filter: hard error even though embed degradation is on.
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.search('docs','body','q',
                     filter => '{\"ghost\": 1}'::jsonb)",
            )
            .ok();
        });
        assert!(
            r.is_err(),
            "filter validation must fail hard before the embed call"
        );
    }

    /// search() (here: its degraded lexical leg) and search_with_vector()
    /// apply the identical filter semantics.
    #[pg_test]
    fn search_with_vector_filter_parity() {
        setup_filtered();
        let via_search = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(pk_value ORDER BY pk_value)
               FROM postvec.search('docs','body','revenue guidance',
                                   filter => '{\"category\": \"finance\"}'::jsonb)",
        )
        .unwrap()
        .unwrap_or_default();
        let via_vector = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(pk_value ORDER BY pk_value)
               FROM postvec.search_with_vector(
                    'docs','body', ARRAY[1,0,0]::real[], 'revenue guidance',
                    semantic_weight => 0, filter => '{\"category\": \"finance\"}'::jsonb)",
        )
        .unwrap()
        .unwrap_or_default();
        // The lexical candidates agree; the vector call may add
        // semantic-only rows, so compare on the keyword hit.
        assert!(via_search.contains(&"1".to_string()));
        assert!(via_vector.contains(&"1".to_string()));
        assert!(!via_search.contains(&"2".to_string()));
        assert!(!via_vector.contains(&"2".to_string()));
    }

    /// The predicate sits inside the candidate CTEs, before their LIMIT: with
    /// candidates => 1, a filtered-out nearest row must not consume the one
    /// candidate slot.
    #[pg_test]
    fn filter_applies_before_candidate_limit() {
        setup_filtered();
        let rows: Vec<(String, Option<i64>)> = Spi::connect(|c| {
            c.select(
                "SELECT pk_value, semantic_rank
                   FROM postvec.search_with_vector(
                        'docs','body', ARRAY[1,0,0]::real[], '',
                        semantic_weight => 1, candidates => 1,
                        filter => '{\"category\": \"office\"}'::jsonb)",
                None,
                &[],
            )
            .unwrap()
            .map(|r| {
                (
                    r.get::<String>(1).unwrap().unwrap(),
                    r.get::<i64>(2).unwrap(),
                )
            })
            .collect()
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "2", "the passing row fills the candidate slot");
        assert_eq!(
            rows[0].1,
            Some(1),
            "it ranks first semantically — the excluded nearer row never entered the CTE"
        );
    }

    /// Operator support is decided by PostgreSQL's parser resolving the exact
    /// rendered expression shape, not by approximating resolution through
    /// `pg_operator`. The approximation rejected every polymorphic operator —
    /// enum equality and array comparisons resolve through `anyenum`/
    /// `anyarray`, which no same-type `pg_operator` row nor implicit cast
    /// describes — so those filters must work.
    #[pg_test]
    fn filter_accepts_polymorphic_operators() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run("CREATE TYPE cc_mood AS ENUM ('calm', 'brisk')").unwrap();
        Spi::run(
            "CREATE TABLE pdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                 body text, mood cc_mood, tags text[])",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO pdocs (body, mood, tags) VALUES
                 ('first', 'calm', ARRAY['a','b']), ('second', 'brisk', ARRAY['c'])",
        )
        .unwrap();
        Spi::get_one::<i64>("SELECT postvec.enable('pdocs','body','m', backfill => false)")
            .unwrap();
        // Both rows need a vector: these assertions count semantic-leg hits.
        Spi::run("UPDATE pdocs SET body_semantic = '[1,0,0]'::vector").unwrap();

        for (filter, expected, why) in [
            ("'{\"mood\": \"calm\"}'", 1i64, "enum equality (anyenum)"),
            ("'{\"mood\": {\"neq\": \"calm\"}}'", 1, "enum inequality"),
            (
                "'{\"mood\": {\"in\": [\"calm\", \"brisk\"]}}'",
                2,
                "enum IN",
            ),
            ("'{\"tags\": \"{a,b}\"}'", 1, "array equality (anyarray)"),
            ("'{\"tags\": {\"gt\": \"{a}\"}}'", 2, "array comparison"),
        ] {
            let q = format!(
                "SELECT count(*) FROM postvec.search_with_vector(
                     'pdocs','body', ARRAY[1,0,0]::real[], '', filter => {filter}::jsonb)"
            );
            assert_eq!(
                Spi::get_one::<i64>(&q).unwrap(),
                Some(expected),
                "{why} must resolve: {filter}"
            );
        }
        // An enum value outside the type is still refused by input validation.
        let r = std::panic::catch_unwind(|| {
            Spi::get_one::<i64>(
                "SELECT count(*) FROM postvec.search_with_vector(
                     'pdocs','body', ARRAY[1,0,0]::real[], '',
                     filter => '{\"mood\": \"frantic\"}'::jsonb)",
            )
            .ok();
        });
        assert!(r.is_err(), "an invalid enum label must refuse");
    }

    /// LIKE/ILIKE patterns get the same exact-declared-type validation as
    /// every other value (typmod and domain constraints included) before the
    /// pattern is bound as text — the `::text` bind exists only so a
    /// `varchar(n)` cast cannot silently truncate the pattern at execution.
    #[pg_test]
    fn filter_like_pattern_is_type_validated() {
        setup_filtered();
        // `title` is varchar(50): a 60-char pattern is invalid input for the
        // exact declared type, exactly as it would be for an equality value.
        let long = "x".repeat(60);
        for op in ["like", "ilike"] {
            let q = format!(
                "SELECT count(*) FROM postvec.search_with_vector(
                     'docs','body', ARRAY[1,0,0]::real[], '',
                     filter => '{{\"title\": {{\"{op}\": \"{long}\"}}}}'::jsonb)"
            );
            let r = std::panic::catch_unwind(|| {
                Spi::get_one::<i64>(&q).ok();
            });
            assert!(
                r.is_err(),
                "an over-long {op} pattern must fail the exact-type check"
            );
        }
        // A pattern within the declared type still works, wildcards included.
        assert_eq!(
            filtered_pks("'{\"title\": {\"like\": \"Q3%\"}}'::jsonb"),
            vec!["1", "3"]
        );
    }

    /// The exact installed signatures carry the pgvector iterative-scan
    /// function-local setting.
    #[pg_test]
    fn search_functions_set_iterative_scan() {
        for sig in [
            "postvec.search(text, text, text, integer, real, integer, integer, jsonb)",
            "postvec.search_with_vector(text, text, real[], text, integer, real, integer, \
             integer, jsonb)",
        ] {
            let config = Spi::get_one_with_args::<Vec<String>>(
                "SELECT proconfig FROM pg_proc WHERE oid = $1::regprocedure",
                &[sig.into()],
            )
            .unwrap()
            .unwrap_or_default();
            assert!(
                config
                    .iter()
                    .any(|c| c == "hnsw.iterative_scan=relaxed_order"),
                "{sig} must carry the iterative-scan setting: {config:?}"
            );
        }
    }

    // ---- Recursive search ----

    /// Three documents × several chunks each, with hand-placed vectors so
    /// ranking is deterministic. Doc 1 holds both the closest AND
    /// second-closest chunks to the probe [1,0,0]; doc 2 the third; doc 3 is
    /// far away but keyword-rich.
    fn setup_chunked() -> RegistryEntry {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE cdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                 category text, body text)",
        )
        .unwrap();
        Spi::run(
            "INSERT INTO cdocs (category, body) VALUES
                 ('a', 'first doc'), ('a', 'second doc'), ('b', 'third doc keyword')",
        )
        .unwrap();
        Spi::get_one::<i64>(
            "SELECT postvec.enable('cdocs','body','m', chunking => 'recursive',
                                   destination => 'cdocs_chunks', backfill => false)",
        )
        .unwrap();
        // Hand-materialize chunks with vectors (the worker is not involved).
        for (pk, seq, text, v) in [
            (1i64, 0, "one alpha", "[1,0,0]"),
            (1, 1, "one beta", "[0.95,0.05,0]"),
            (2, 0, "two gamma", "[0.9,0.1,0]"),
            (3, 0, "three keyword", "[0,0,1]"),
        ] {
            Spi::run_with_args(
                "INSERT INTO cdocs_chunks
                     (postvec_source_pk, postvec_chunk_seq, postvec_char_start,
                      postvec_char_end, chunk_text, body_semantic)
                 VALUES ($1, $2, 0, 9, $3, $4::vector)",
                &[pk.into(), seq.into(), text.into(), v.into()],
            )
            .unwrap();
        }
        RegistryEntry::load_active("public", "cdocs", "body").unwrap()
    }

    /// One result per source PK despite several high-ranking chunks, with the
    /// deterministic winning chunk's metadata appended.
    #[pg_test]
    fn chunk_search_collapses_to_one_row_per_document() {
        let entry = setup_chunked();
        let q = [1.0f32, 0.0, 0.0];
        let rows = super::hybrid_rows(
            &entry,
            Some(&q),
            "no keyword matches this",
            10,
            0.5,
            60,
            200,
            &super::RenderedFilter::none(),
        );
        let pks: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(
            pks.iter().filter(|p| **p == "1").count(),
            1,
            "doc 1's two high-ranking chunks collapse to one row: {pks:?}"
        );
        assert_eq!(rows[0].0, "1", "doc 1 wins (closest chunk)");
        assert_eq!(rows[0].4, Some(0), "the winning chunk is seq 0");
        assert_eq!(rows[0].7.as_deref(), Some("one alpha"));
        assert_eq!((rows[0].5, rows[0].6), (Some(0), Some(9)));
        assert_eq!(rows[1].0, "2", "doc 2 second");
    }

    /// Document score is the maximum of one chunk, not the sum — a document
    /// with many mediocre chunks does not beat one great chunk.
    #[pg_test]
    fn document_score_is_max_not_sum() {
        let entry = setup_chunked();
        // Give doc 3 many mediocre chunks near the probe, all worse than doc
        // 1's best.
        for seq in 1..=8 {
            Spi::run_with_args(
                "INSERT INTO cdocs_chunks
                     (postvec_source_pk, postvec_chunk_seq, postvec_char_start,
                      postvec_char_end, chunk_text, body_semantic)
                 VALUES (3, $1, 0, 4, 'pad', '[0.5,0.5,0]'::vector)",
                &[seq.into()],
            )
            .unwrap();
        }
        let q = [1.0f32, 0.0, 0.0];
        let rows = super::hybrid_rows(
            &entry,
            Some(&q),
            "zzz",
            10,
            1.0,
            60,
            200,
            &super::RenderedFilter::none(),
        );
        assert_eq!(
            rows[0].0, "1",
            "one excellent chunk beats eight mediocre ones"
        );
    }

    /// The degraded FTS-only path searches chunk text and still collapses.
    #[pg_test]
    fn chunk_fts_degraded_path_searches_chunk_text() {
        let entry = setup_chunked();
        let rows = super::hybrid_rows(
            &entry,
            None,
            "keyword",
            10,
            0.5,
            60,
            200,
            &super::RenderedFilter::none(),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "3", "lexical hit on chunk text");
        assert!(rows[0].2.is_none(), "no semantic rank in degraded mode");
        assert_eq!(rows[0].7.as_deref(), Some("three keyword"));
    }

    /// Metadata filters constrain both chunk legs before fusion.
    #[pg_test]
    fn chunk_search_applies_p6_filter_to_source() {
        let entry = setup_chunked();
        let rel_oid = Spi::get_one::<pg_sys::Oid>("SELECT 'cdocs'::regclass::oid")
            .unwrap()
            .unwrap();
        let filter_json: serde_json::Value = serde_json::json!({"category": "b"});
        let rendered = super::render_filter(rel_oid, super::SEARCH_ALIAS, Some(&filter_json), 3);
        let q = [1.0f32, 0.0, 0.0];
        let rows = super::hybrid_rows(&entry, Some(&q), "keyword", 10, 0.5, 60, 200, &rendered);
        assert_eq!(rows.len(), 1, "only category-b documents survive");
        assert_eq!(rows[0].0, "3");
    }

    /// Source RLS hides destination rows in the direct table, the view, and
    /// both search paths for an application role.
    #[pg_test]
    fn source_rls_constrains_chunks_everywhere() {
        setup_chunked();
        Spi::run("CREATE ROLE chunk_app LOGIN").unwrap();
        Spi::run("GRANT USAGE ON SCHEMA public TO chunk_app").unwrap();
        Spi::run("GRANT SELECT ON cdocs, cdocs_chunks, cdocs_chunks_view TO chunk_app").unwrap();
        // The app sees only doc 1 through source RLS.
        Spi::run("ALTER TABLE cdocs ENABLE ROW LEVEL SECURITY").unwrap();
        Spi::run("CREATE POLICY only_one ON cdocs FOR SELECT USING (id = 1)").unwrap();

        Spi::run("SET ROLE chunk_app").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM cdocs_chunks").unwrap(),
            Some(2),
            "direct destination reads pass through the FORCE-RLS source policy"
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM cdocs_chunks_view").unwrap(),
            Some(2),
            "the invoker view applies caller RLS"
        );
        let pks = Spi::get_one::<i64>(
            "SELECT count(DISTINCT pk_value) FROM postvec.search_with_vector(
                 'cdocs','body', ARRAY[1,0,0]::real[], query_text => 'keyword')",
        )
        .unwrap();
        assert_eq!(pks, Some(1), "search sees only RLS-visible documents");
        let hidden = Spi::get_one::<i64>(
            "SELECT count(*) FROM postvec.search_with_vector(
                 'cdocs','body', ARRAY[1,0,0]::real[], query_text => 'keyword')
              WHERE pk_value <> '1'",
        )
        .unwrap();
        assert_eq!(hidden, Some(0));
        Spi::run("RESET ROLE").unwrap();
    }

    /// The SQL-level search functions return the appended chunk metadata for
    /// a recursive entry, and column-mode entries return NULLs there.
    #[pg_test]
    fn search_result_shape_appends_chunk_metadata() {
        setup_chunked();
        let row = Spi::get_one::<bool>(
            "SELECT chunk_seq IS NOT NULL AND chunk_start IS NOT NULL
                    AND chunk_end IS NOT NULL AND chunk_text IS NOT NULL
               FROM postvec.search_with_vector('cdocs','body', ARRAY[1,0,0]::real[])
              LIMIT 1",
        )
        .unwrap();
        assert_eq!(row, Some(true));
    }

    /// EXPLAIN gate: the recursive semantic CTE shape can ride the
    /// destination HNSW index — unfiltered and with a moderately selective
    /// source filter — instead of a full vector sort. (Tiny test tables make
    /// the planner prefer a seq scan on cost; disabling it proves the index
    /// path exists and is planable, which is what the ≥100k-row spike showed
    /// it wins on naturally.)
    #[pg_test]
    fn chunk_semantic_shape_can_use_hnsw() {
        let _entry = setup_chunked();
        Spi::run(
            "INSERT INTO cdocs_chunks
                 (postvec_source_pk, postvec_chunk_seq, postvec_char_start,
                  postvec_char_end, chunk_text, body_semantic)
             SELECT 2, 100 + g, 0, 4, 'fill', ('[' || random()::real || ',0.2,0.1]')::vector
               FROM generate_series(1, 60) g",
        )
        .unwrap();
        Spi::run("CREATE INDEX cdocs_chunks_hnsw ON cdocs_chunks USING hnsw (body_semantic vector_cosine_ops)")
            .unwrap();
        // A ~70-row table makes exact top-N plans cheaper on cost, so the
        // planner's natural choice here is not the spike's; disabling seq
        // scans and sorts leaves the ordered ANN path as the only way to
        // satisfy the ORDER BY — proving the shape is index-servable (a
        // shape that broke HNSW compatibility would fail to plan around it
        // and fall back to a sort anyway, failing the assertions below).
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        Spi::run("SET LOCAL enable_sort = off").unwrap();
        for filter in ["", "AND d.category = 'a'"] {
            let plan = Spi::connect(|c| {
                let t = c
                    .select(
                        &format!(
                            "EXPLAIN (COSTS OFF)
                             SELECT c.postvec_chunk_id
                               FROM cdocs_chunks c JOIN cdocs d ON d.id = c.postvec_source_pk
                              WHERE c.body_semantic IS NOT NULL {filter}
                              ORDER BY c.body_semantic <=> '[1,0,0]'::vector
                              LIMIT 5"
                        ),
                        None,
                        &[],
                    )
                    .unwrap();
                t.into_iter()
                    .map(|r| r.get::<String>(1).unwrap().unwrap())
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            assert!(
                plan.contains("cdocs_chunks_hnsw"),
                "the semantic shape must be servable by the HNSW index (filter={filter:?}):\n{plan}"
            );
            assert!(
                !plan.contains("Sort Key"),
                "no full vector sort (filter={filter:?}):\n{plan}"
            );
        }
    }

    /// The chunk-text GIN index serves the lexical leg's expression.
    #[pg_test]
    fn chunk_fts_expression_matches_gin_index() {
        Spi::run(
            "INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
             VALUES ('m','embed','m',3,'{}'::jsonb) ON CONFLICT (name) DO NOTHING",
        )
        .unwrap();
        Spi::run(
            "CREATE TABLE gdocs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text)",
        )
        .unwrap();
        let id = Spi::get_one::<i64>(
            "SELECT postvec.enable('gdocs','body','m', chunking => 'recursive',
                                   destination => 'gdocs_chunks',
                                   create_fts_index => true, backfill => false)",
        )
        .unwrap()
        .unwrap();
        // The generated GIN index is on the destination chunk_text and is
        // extension-stamped.
        let def = Spi::get_one::<String>(&format!(
            "SELECT pg_get_indexdef(('postvec_fts_{id}')::regclass)"
        ))
        .unwrap()
        .unwrap_or_default();
        assert!(
            def.contains("gdocs_chunks") && def.contains("chunk_text") && def.contains("gin"),
            "{def}"
        );
        Spi::run(
            "INSERT INTO gdocs_chunks
                 (postvec_source_pk, postvec_chunk_seq, postvec_char_start,
                  postvec_char_end, chunk_text)
             SELECT 1, g, 0, 4, 'word ' || g FROM generate_series(0, 200) g",
        )
        .unwrap();
        Spi::run("INSERT INTO gdocs (body) VALUES ('word 7')").unwrap();
        Spi::run("SET LOCAL enable_seqscan = off").unwrap();
        let plan = Spi::connect(|c| {
            let t = c
                .select(
                    "EXPLAIN (COSTS OFF)
                     SELECT 1 FROM gdocs_chunks c
                      WHERE to_tsvector('pg_catalog.english'::regconfig, c.chunk_text)
                            @@ websearch_to_tsquery('pg_catalog.english'::regconfig, 'word')",
                    None,
                    &[],
                )
                .unwrap();
            t.into_iter()
                .map(|r| r.get::<String>(1).unwrap().unwrap())
                .collect::<Vec<_>>()
                .join("\n")
        });
        assert!(
            plan.contains(&format!("postvec_fts_{id}")),
            "the lexical expression must match the generated GIN index:\n{plan}"
        );
    }
}

#[cfg(test)]
mod unit_tests {
    use super::{semantic_match_exprs, validate_query_embedding};
    use crate::registry::RegistryEntry;

    fn entry(dim: i32) -> RegistryEntry {
        RegistryEntry {
            id: 1,
            table_schema: "public".into(),
            table_name: "docs".into(),
            source_column: "body".into(),
            vector_column: "body_semantic".into(),
            pk_columns: vec!["id".into()],
            pk_types: vec!["bigint".into()],
            model: "m".into(),
            dim,
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

    /// The model's query embedding is validated before it reaches dynamic
    /// SQL: wrong cardinality, wrong dimension and non-finite components
    /// all fail with a clear reason.
    #[test]
    fn query_embedding_validation() {
        let e = entry(3);
        assert!(
            validate_query_embedding(&e, vec![]).is_err(),
            "zero embeddings"
        );
        assert!(
            validate_query_embedding(&e, vec![vec![1.0, 0.0, 0.0], vec![0.0, 1.0, 0.0]]).is_err(),
            "two embeddings for one query"
        );
        assert!(
            validate_query_embedding(&e, vec![vec![1.0, 0.0]]).is_err(),
            "wrong dimension"
        );
        assert!(
            validate_query_embedding(&e, vec![vec![f32::NAN, 0.0, 0.0]]).is_err(),
            "NaN component"
        );
        assert_eq!(
            validate_query_embedding(&e, vec![vec![1.0, 2.0, 3.0]]).unwrap(),
            vec![1.0, 2.0, 3.0]
        );
    }

    #[test]
    fn semantic_exprs_switch_to_halfvec_above_2000_dims() {
        let (sem, qparam) = semantic_match_exprs(&entry(1024), "");
        assert_eq!(sem, "\"body_semantic\"");
        assert_eq!(qparam, "$1::vector");

        let (sem, qparam) = semantic_match_exprs(&entry(2100), "");
        assert_eq!(sem, "(\"body_semantic\"::halfvec(2100))");
        assert_eq!(qparam, "$1::halfvec(2100)");
    }
}
