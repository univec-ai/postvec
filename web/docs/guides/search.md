---
title: Search
description: Hybrid RRF search with postvec.search() and search_with_vector().
---

# Search

`search()` runs vector retrieval and full-text retrieval, then fuses the
two with Reciprocal Rank Fusion. Matching primary keys come back as
text, so the join works on any table shape.

Wait until `pending_jobs = 0` before judging ranks. A row whose vector
is still NULL matches on the lexical (BM25) leg only.

:::: code-group

```sql [search]
SELECT d.id, d.body,
       round(s.rrf_score::numeric, 5) AS score,
       s.semantic_rank, s.fts_rank
  FROM postvec.search(
         'public.docs', 'body',
         'switching AI models without redoing the work'
       ) AS s
  JOIN public.docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

```sql [search_with_vector]
SELECT d.id, d.body, s.rrf_score
  FROM postvec.search_with_vector(
         'public.docs', 'body',
         postvec.embed(
           'switching AI models without redoing the work',
           'sentence-transformers-all-minilm-l6-v2'
         ),
         query_text => 'switching AI models without redoing the work'
       ) AS s
  JOIN public.docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

::::

:::: tip Expected
Rows that mean the same thing rank above rows that merely share a word.
`semantic_rank` / `fts_rank` are 1-based positions in each leg; either
can be NULL if that leg missed the row. `semantic_distance` is the raw
pgvector distance in the entry's metric (`<=>`, `<->` or `<#>`) and
`fts_score` is [BM25](/docs/guides/bm25) (or `ts_rank_cd` until the
first lexical-stats refresh), for thresholds and debugging. Ties on
`rrf_score` order by `pk_value`, so paging is stable.
::::

Cast `pk_value` back to the PK type (`::bigint`, `::uuid`, ...).

## Arguments

| Argument | Default | Meaning |
|---|---|---|
| `limit_n` | 10 | Rows returned, 1 to 1000 |
| `semantic_weight` | 0.5 | 1.0 = vector only, 0.0 = BM25 only. See [BM25](/docs/guides/bm25) |
| `rrf_k` | 60 | RRF constant, 1 or more |
| `candidates` | derived | Pool size per leg before fusion, 1 to 100000. Derived from `limit_n` as `limit_n * 4` (at least 50), or `limit_n * 16` (at least 200) for chunked entries |
| `filter` | none | [Typed metadata](/docs/guides/filters) |

Chunked entries also return `chunk_seq`, `chunk_start`, `chunk_end`,
`chunk_text` for the **winning** chunk: one row per document.
Column-mode entries leave those NULL.

The `chunk_text` returned by one call is capped at 64 MiB in total. Rows past
that ceiling keep their rank and return `chunk_text` NULL, with a WARNING.
Lower `limit_n`, or re-create the entry with a smaller `chunk_size`.

## Search with a supplied vector

The `search_with_vector` tab is the same join, with a vector you already
have. `query_text` still feeds the [BM25](/docs/guides/bm25) leg. Omit
it (default `''`) for a vector-only search. Grant `embed()` explicitly,
or pass a vector computed elsewhere.

## Search is slow

The default `index_mode` is `manual`, so no ANN index exists until one
is created.

```sql
SELECT has_vector_index, index_error FROM postvec.status();
SELECT postvec.create_vector_index('public.docs', 'body');
```

See [indexes](/docs/guides/indexes). A missing FTS index only hurts the
lexical leg. `create_fts_index => true` at enable time builds a GIN.
Keyword traffic needs it the same way ANN traffic needs HNSW.

After a bulk load, refresh BM25 corpus statistics before judging ranks:

```sql
SELECT postvec.refresh_lexical_stats('public.docs', 'body');
```

How scoring, stats, RLS and the GIN interact: [BM25](/docs/guides/bm25).

## Lexical-only results

When query embedding fails, `postvec.search_degrade_to_fts` is on by
default and the search continues on the lexical leg. `semantic_rank` is
NULL on every row. Fix the inference failure, or set
`postvec.search_degrade_to_fts = off` so the same failure returns an
error.

## Query embedding is synchronous

`search()` embeds the **query** inline. Document vectors still fill after
commit. See [eventual consistency](/docs/concepts/consistency).

For a column whose stored model is retired or provider-only,
[search a retired space](/docs/guides/bridge) converts each query into
that space (embed-bridge). Stored rows stay.

On [managed PostgreSQL](/docs/server/managed), `search(text)` and
`embed()` go through the postvec-server proxy. `search_with_vector()`
runs on the database directly. In remote mode on a self-hosted cluster,
query embedding is served by [postvec-server](/docs/server/) the same
way document embedding is.
