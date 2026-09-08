---
title: Coming from pgai
description: Map a pgai or pg_vectorize pipeline onto postvec enable, adopt, search and migrate.
---

# Coming from pgai

Timescale archived [pgai](https://github.com/timescale/pgai) in May 2026
and Tiger Cloud removed the managed vectorizer. [pg_vectorize](https://github.com/ChuckHend/pg_vectorize)
is a similar in-database embedding pipeline. This page maps those
surfaces onto postvec.

Existing `vector(N)` columns stay as they are. [`adopt()`](/docs/guides/adopt)
registers them; [`search()`](/docs/guides/search) ranks in that stored
space. [`migrate()`](/docs/guides/migrate) is a later step if you want a
different stored model.

## Concept map

| pgai / pg_vectorize | postvec |
|---|---|
| `ai.create_vectorizer(...)` / `vectorize.table(...)` | [`enable()`](/docs/guides/enable) on a text column |
| Destination embedding table | Shadow `{column}_semantic` on the same table, or a [chunk destination](/docs/guides/chunking) |
| OpenAI / Cohere / Ollama / ... embedder | A local model, or an [external provider](/docs/models/providers) (`provider add`) |
| Recursive character splitter | `chunking => 'recursive'` plus `destination` |
| Python format template | [`format =>`](/docs/guides/templates) / `set_format()` |
| Scheduling / background worker | The postvec worker (preload + restart, or [postvec-server](/docs/server/) on managed hosts) |
| `ORDER BY embedding <=> embed(query)` / `vectorize.search()` | [`search()`](/docs/guides/search): hybrid RRF, [BM25](/docs/guides/bm25) and [filters](/docs/guides/filters) in one call |
| Re-run the vectorizer to change model | [`migrate()`](/docs/guides/migrate) converts stored vectors in place |
| Drop the vectorizer | [`disable()`](/docs/guides/enable#disable) |
| API keys in GUCs or catalog tables | `0600` files in `providers.d` on the inference host |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | [Managed PostgreSQL](/docs/server/managed) on postvec-server |

SQL lives in the `postvec` schema. Qualify every call.

## Existing vectors

If the table already has a populated `vector(N)` column, register it and
search that space. The model name is the space those bytes came from:

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'openai-text-embedding-ada-002',
  backfill => 'none'
);
```

:::: tip Expected
`owns_vector_column` is false. Stored bytes stay. Future writes stay in
sync when `sync => true` (the default).
::::

For a retired or provider-only space, [search the existing space](/docs/guides/bridge)
converts each query into that space (embed-bridge). A local converter
chain, or a later [`migrate()`](/docs/guides/migrate), is independent of
how the column was originally filled.

## New column

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2',
  create_fts_index => true
);
```

Wait until `pending_jobs = 0`, then:

```sql
SELECT postvec.create_vector_index('public.docs', 'body');

SELECT d.id, d.body, s.rrf_score, s.semantic_rank, s.fts_rank
  FROM postvec.search(
         'public.docs', 'body',
         'switching models without redoing the work'
       ) AS s
  JOIN public.docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

A hosted OpenAI (or similar) column uses the SQL name from
`provider ls` after [`provider add`](/docs/models/providers). The key
stays in `providers.d`.

Long documents: [chunking](/docs/guides/chunking) (`chunking => 'recursive'`).
Title or other row context: [templates](/docs/guides/templates).

## Search

`search()` fuses vector retrieval and BM25, then returns matching
primary keys. Join back to the source table. `semantic_weight => 0.0`
is keyword-only; `1.0` is vector-only. Default `0.5` is hybrid.

pgai and pg_vectorize typically returned rows from a destination table
or a helper that already joined. postvec returns `pk_value` plus scores;
the join is yours, so it works on any table shape.

## Change the stored model

pgai's usual path was to drop and recreate the vectorizer (full
re-embed). postvec can convert stored vectors:

```sql
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3'
) AS migration_id \gset

SELECT state, rows_done, rows_total
  FROM postvec.migration_status(:migration_id);
-- until state = 'awaiting_finalize'

SELECT postvec.migration_finalize(:migration_id);
```

Default `strategy` is `convert`. `reembed` embeds current source text
with the new model. Confirm provenance at adopt time before converting.
[Change the stored model](/docs/guides/migrate).

## Where inference runs

| Host | Path |
|---|---|
| Self-hosted PostgreSQL | [Packages](/docs/install/packages) then [configure](/docs/install/setup). Embedded by default. |
| GPU, process isolation, a fleet, dashboard | [postvec-server](/docs/server/) in remote mode. SQL is unchanged. |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | [Managed PostgreSQL](/docs/server/managed): plain SQL schema, worker in postvec-server |

pg_vectorize already used an HTTP worker plus a wire-protocol proxy on
managed hosts. postvec-server is that role here: schema install, sync
worker and an optional `search(text)` proxy.

## License

The extension is PostgreSQL-licensed. postvec-server is Business Source
License 1.1. Personal use, non-production and a 30-day production
evaluation are free. [License](/docs/license).

- [SQL functions](/docs/guides/)
- [Adopt](/docs/guides/adopt)
- [Search a retired space](/docs/guides/bridge)
- [postvec-server](/docs/server/)
- [Managed PostgreSQL](/docs/server/managed)
