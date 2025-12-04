---
title: Search
description: Hybrid RRF search with postvec.search() and search_with_vector().
---

# Search

One function, two legs (vector + full-text), fused with Reciprocal Rank
Fusion. You get the matching primary key as text; you join back.

```sql
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

::: tip Expected
Rows that mean the same thing rank above rows that merely share a word.
`semantic_rank` / `fts_rank` are 1-based positions in each leg; either
can be NULL if that leg missed the row.
:::

## Arguments

| Argument | Default | Meaning |
|---|---|---|
| `limit_n` | 10 | Rows returned |
| `semantic_weight` | 0.5 | 1.0 = vector only, 0.0 = FTS only |
| `rrf_k` | 60 | RRF constant |
| `candidates` | derived | Pool size per leg before fusion |
| `filter` | none | [Typed metadata](/docs/guides/filters) |

Chunked entries also return `chunk_seq`, `chunk_start`, `chunk_end`,
`chunk_text` for the **winning** chunk — one row per document. Column-mode
entries leave those NULL.

## Bring your own vector

```sql
SELECT d.id, d.body, s.rrf_score
  FROM postvec.search_with_vector(
         'public.docs', 'body',
         postvec.embed(
           'watering the plants',
           'sentence-transformers-all-minilm-l6-v2'
         ),
         query_text => 'watering the plants'
       ) AS s
  JOIN public.docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

`query_text` still feeds the FTS leg. Omit it (default `''`) for a
vector-only search. `embed()` is not PUBLIC — grant it, or pass a vector
you computed elsewhere.

## When it is slow

There is no ANN index. Default `index_mode` is `manual` on purpose.

```sql
SELECT has_vector_index, index_error FROM postvec.status();
SELECT postvec.create_vector_index('public.docs', 'body');
```

See [indexes](/docs/guides/indexes). A missing FTS index only hurts the
lexical leg; `create_fts_index => true` at enable time builds one.

## When it looks "FTS only"

Query embedding failed and `postvec.search_degrade_to_fts` is on (default).
`semantic_rank` is NULL on every row. Fix inference, or `SET
postvec.search_degrade_to_fts = off` so the failure is loud.

## Don't

::: danger Don't treat `search()` as `SELECT * FROM docs`
It returns rank diagnostics, not the row. Join on `pk_value`. Cast it
back to the PK type (`::bigint`, `::uuid`, …).
:::

::: danger Don't call `search()` and expect a filled vector on a row you
just inserted in this transaction
The worker cannot write back until you commit. See
[eventual consistency](/docs/concepts/consistency).
:::
