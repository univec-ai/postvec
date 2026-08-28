// SPDX-License-Identifier: PostgreSQL
use crate::registry::{quote_ident, quote_literal, RegistryEntry};
pub fn semantic_match_exprs(entry: &RegistryEntry, alias: &str) -> (String, String) {
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
pub const SEARCH_ALIAS: &str = "d";

/// The lexical leg's per-query CTE: query lexemes with their document
/// frequencies and the entry's corpus stats (`postvec._lexical_terms`).
fn stats_cte(entry: &RegistryEntry) -> String {
    let cfg = quote_literal(&entry.fts_config);
    format!(
        "stats AS (SELECT * FROM postvec._lexical_terms({}, {cfg}::regconfig, $2))",
        entry.id
    )
}

#[allow(clippy::too_many_arguments)]
/// `fts` candidate CTE: `@@` matches scored by BM25 (`ts_rank_cd` before the
/// first stats refresh), top `candidates` ranked with a deterministic tiebreak.
fn fts_cte(
    name: &str,
    fields: &str,
    from: &str,
    tsv: &str,
    cfg: &str,
    key: &str,
    fpred: &str,
    candidates: i32,
) -> String {
    let tsq = format!("websearch_to_tsquery({cfg}::regconfig, $2)");
    format!(
        "{name} AS (
             SELECT *, row_number() OVER (ORDER BY score_fts DESC, {key}) AS rank_fts
               FROM (
                    SELECT {fields},
                           postvec.lexical_score({tsv}, {tsq}, st.terms, st.dfs, st.n, st.avgdl) AS score_fts
                      FROM {from}, stats st
                     WHERE {tsv} @@ {tsq}{fpred}
                     ORDER BY score_fts DESC, {key}
                     LIMIT {candidates}
               ) scored
         )"
    )
}

#[allow(clippy::too_many_arguments)]
pub fn hybrid_rows_sql(
    entry: &RegistryEntry,
    has_vector: bool,
    limit_n: i32,
    weight: f32,
    rrf_k: i32,
    candidates: i32,
    predicate: Option<&str>,
) -> String {
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
    let fpred = predicate
        .map(|p| format!("\n                AND ({p})"))
        .unwrap_or_default();
    let prep = stats_cte(entry);
    let fts_cte = fts_cte(
        "fts",
        &format!("{pk} AS pk"),
        &format!("{tbl} {d}"),
        &format!("to_tsvector({cfg}::regconfig, {col}::text)"),
        &cfg,
        "pk",
        &fpred,
        candidates,
    );

    if has_vector {
        format!(
            "WITH {prep},
             semantic AS (
                 SELECT {pk} AS pk, ({sem} {op} {qparam})::float8 AS dist,
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
                        s.rank_sem, f.rank_fts, s.dist, f.score_fts
                   FROM semantic s FULL OUTER JOIN fts f USING (pk)
             )
             SELECT pk, rrf_score, rank_sem, rank_fts, dist, score_fts
               FROM fused ORDER BY rrf_score DESC NULLS LAST, pk LIMIT {limit_n}"
        )
    } else {
        format!(
            "WITH {prep},
             {fts_cte},
             fused AS (
                 SELECT pk, (1.0 / ({k} + rank_fts))::float8 AS rrf_score,
                        NULL::bigint AS rank_sem, rank_fts, NULL::float8 AS dist, score_fts
                   FROM fts
             )
             SELECT pk, rrf_score, rank_sem, rank_fts, dist, score_fts
               FROM fused ORDER BY rrf_score DESC, pk LIMIT {limit_n}"
        )
    }
}

#[allow(clippy::too_many_arguments)]
pub fn chunk_hybrid_rows_sql(
    entry: &RegistryEntry,
    has_vector: bool,
    limit_n: i32,
    weight: f32,
    rrf_k: i32,
    candidates: i32,
    predicate: Option<&str>,
) -> String {
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
    let fpred = predicate
        .map(|p| format!("\n                AND ({p})"))
        .unwrap_or_default();
    let prep = stats_cte(entry);

    // Deliberately NO chunk_text here: projecting it through the candidate
    // and fusion CTEs would materialize candidate-pool × chunk-size bytes in
    // the SPI result before any Rust-side ceiling could run. The winners'
    // text is fetched in a second, byte-budgeted phase below.
    let chunk_fields = "c.postvec_chunk_id AS cid, c.postvec_source_pk::text AS pk,
                    c.postvec_chunk_seq AS seq, c.postvec_char_start AS cs,
                    c.postvec_char_end AS ce";
    let fts_cte = fts_cte(
        "fts_chunks",
        chunk_fields,
        &format!("{qdest} c JOIN {qsrc} {d} ON {src_join}"),
        &format!("to_tsvector({cfg}::regconfig, c.chunk_text)"),
        &cfg,
        "cid",
        &fpred,
        candidates,
    );
    let doc_window = "row_number() OVER (
                        PARTITION BY pk
                        ORDER BY rrf_score DESC,
                                 COALESCE(rank_sem, 9223372036854775807),
                                 COALESCE(rank_fts, 9223372036854775807),
                                 seq, cid) AS doc_row";

    if has_vector {
        format!(
            "WITH {prep},
             semantic_chunks AS (
                 SELECT {chunk_fields}, ({sem} {op} {qparam})::float8 AS dist,
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
                        s.rank_sem, f.rank_fts, s.dist, f.score_fts
                   FROM semantic_chunks s FULL OUTER JOIN fts_chunks f USING (cid)
             ),
             best_per_document AS (
                 SELECT *, {doc_window} FROM fused_chunks
             )
             SELECT cid, pk, rrf_score, rank_sem, rank_fts, seq, cs, ce, dist, score_fts
               FROM best_per_document
              WHERE doc_row = 1
              ORDER BY rrf_score DESC NULLS LAST, pk LIMIT {limit_n}"
        )
    } else {
        format!(
            "WITH {prep},
             {fts_cte},
             fused_chunks AS (
                 SELECT cid, pk, seq, cs, ce,
                        (1.0 / ({k} + rank_fts))::float8 AS rrf_score,
                        NULL::bigint AS rank_sem, rank_fts, NULL::float8 AS dist, score_fts
                   FROM fts_chunks
             ),
             best_per_document AS (
                 SELECT *, {doc_window} FROM fused_chunks
             )
             SELECT cid, pk, rrf_score, rank_sem, rank_fts, seq, cs, ce, dist, score_fts
               FROM best_per_document
              WHERE doc_row = 1
              ORDER BY rrf_score DESC, pk LIMIT {limit_n}"
        )
    }
}
