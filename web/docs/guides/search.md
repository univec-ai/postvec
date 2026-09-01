---
title: Search
description: Hybrid RRF search with postvec.search() and search_with_vector().
---

# Search

`search()` runs vector retrieval and full-text retrieval, then fuses the
two with Reciprocal Rank Fusion. Matching primary keys come back as
text, so the join works on any table shape.

Wait until `pending_jobs = 0` before judging ranks. A row whose vector
is still NULL matches on the lexical leg only.

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
`fts_score` is BM25 (or `ts_rank_cd` until the first lexical-stats
refresh), for thresholds and debugging. Ties on `rrf_score` order by
`pk_value`, so paging is stable.
::::

Cast `pk_value` back to the PK type (`::bigint`, `::uuid`, ...).

## Arguments

| Argument | Default | Meaning |
|---|---|---|
| `limit_n` | 10 | Rows returned |
| `semantic_weight` | 0.5 | 1.0 = vector only, 0.0 = FTS only |
| `rrf_k` | 60 | RRF constant |
| `candidates` | derived | Pool size per leg before fusion |
| `filter` | none | [Typed metadata](/docs/guides/filters) |

Chunked entries also return `chunk_seq`, `chunk_start`, `chunk_end`,
`chunk_text` for the **winning** chunk: one row per document.
Column-mode entries leave those NULL.

## Search with a supplied vector

The `search_with_vector` tab is the same join, with a vector you already
have. `query_text` still feeds the FTS leg. Omit it (default `''`) for
a vector-only search. Grant `embed()` explicitly, or pass a vector
computed elsewhere.

## Search is slow

The default `index_mode` is `manual`, so no ANN index exists until one
is created.

```sql
SELECT has_vector_index, index_error FROM postvec.status();
SELECT postvec.create_vector_index('public.docs', 'body');
```

See [indexes](/docs/guides/indexes). A missing FTS index only hurts the
lexical (BM25) leg. `create_fts_index => true` at enable time builds a
GIN; keyword traffic needs it the same way ANN traffic needs HNSW.

After a bulk load, refresh corpus statistics before judging ranks:

```sql
SELECT postvec.refresh_lexical_stats('public.docs', 'body');
```

The worker otherwise rebuilds the stats on its own once the table's
tuple counters have moved and the previous refresh is old enough: at
least 30 s, or ten times as long as that refresh took, so a large corpus
is re-tokenized rarely. A refresh is one `to_tsvector` pass over the
text; it runs inside the worker and delays embedding for that long.
Until the first successful refresh the leg scores with `ts_rank_cd`.
A failed background refresh preserves the previous good BM25 statistics;
check `status().lexical_error`. Automatic retries back off for ten minutes.
Manual refresh raises on failure; its error record rolls back with the
failed statement.
With row-level security enabled on the source, search uses `ts_rank_cd`
and does not expose global term frequencies — except for roles that
already see every row (table owner unless FORCE RLS, `BYPASSRLS`,
superuser). Ordinary readers can see corpus statistics in `status()`
only when they would get BM25.

BM25 scores positive query lexemes; `OR`, exclusions and quoted phrases
keep PostgreSQL's `websearch_to_tsquery` matching semantics. A purely
negative query has no positive BM25 terms and ties at zero. Statistics
cover non-NULL documents (chunks for recursive entries), including empty
and stopword-only text. Metadata filters do not redefine the corpus.
Term frequencies use stored tsvector positions: at most 256 per lexeme,
with positions capped at 16383. This is BM25 over PostgreSQL text search,
not an exact reproduction of another engine's tokenizer or length norms.

Automatic change detection requires `track_counts = on`. Vector updates
also count, so backfills can cause redundant, throttled refreshes. TRUNCATE
alone does not move the tuple counters; refresh manually after truncation,
partition changes or text-search dictionary changes when immediate stats
accuracy matters. Otherwise the next counted write triggers a refresh.

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
