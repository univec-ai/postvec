---
title: SQL functions
description: Match enable, adopt, bridge, migrate or chunking to the table you have.
---

# SQL functions

| Starting point | Call | Result |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | postvec creates and maintains a shadow `vector(N)` column |
| A populated `vector(N)` column, same model | [`adopt()`](/docs/guides/adopt) | Existing bytes stay. Missing rows can backfill. |
| Vectors in a retired or provider-only space | [Bridge search](/docs/guides/bridge) | Queries convert into that space. The corpus stays. |
| Ready to change model | [`migrate()`](/docs/guides/migrate) | Stored vectors convert in place, or re-embed if you choose that. |
| Long source documents | [Recursive chunking](/docs/guides/chunking) | A managed 1:N destination stores passage vectors. Search returns documents. |
| Hosted embedding API (OpenAI, Gemini, ...) | [External providers](/docs/models/providers), then `enable()` | A connector file on the inference side. The key stays out of PostgreSQL |

Functions live in the `postvec` schema (`postvec.enable(...)`).
Leave `search_path` as it is.

## Text, no vectors

`enable()` adds `{column}_semantic`, installs enqueue triggers and
queues existing rows.

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2',
  create_fts_index => true
);
```

1. Wait until `pending_jobs = 0` in [`status()`](/docs/guides/status).
2. [`create_vector_index()`](/docs/guides/indexes).
3. [`search()`](/docs/guides/search).

:::: tip Expected
`enable()` returns a registry id. `docs.body_semantic` fills with
`vector(384)` for MiniLM. `search()` then ranks by meaning and by
keywords.
::::

## A populated vector column

`adopt()` registers the existing column and leaves stored bytes as they
are. The `model` argument records provenance - a wrong name makes
later search and conversion silently invalid.

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'sentence-transformers-all-minilm-l6-v2'
);
```

:::: tip Expected
`owns_vector_column` is false. Only NULL vectors are queued
(`backfill => 'missing'`). Future writes stay in sync.
::::

## Keep a retired space

Name the original model at adopt time. `search()` embeds the query with
an available local model and converts that one vector into the stored
space.

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'openai-text-embedding-ada-002',
  backfill => 'none'
);
```

The converter (and its embed dependency) must be present. On an
embedded host that is [`postvec model pull`](/docs/models/pull). Full
walkthrough: [search a retired space](/docs/guides/bridge).

:::: tip Expected
Existing rows are untouched. Semantic ranks are present. The query
vector has the stored dimension (1536 for classic ada-002).
::::

## Change the stored model

```sql
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3'
);
```

Default strategy is `convert`: stored vectors are translated. The
migration **stops and waits** at `awaiting_finalize`. You swap columns
with `migration_finalize()`. See [migrate](/docs/guides/migrate).

:::: tip Expected
`migration_status()` reaches `awaiting_finalize`. After the first
finalize the column is live on the new model. If it had an ANN index,
a second finalize follows the rebuild.
::::

## Long documents

```sql
SELECT postvec.enable(
  'public.articles', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2',
  chunking => 'recursive',
  destination => 'articles_body_chunks',
  format => E'$title\n\n$chunk'
);
```

Search still returns one row per document, plus the winning chunk. See
[chunking](/docs/guides/chunking).

:::: tip Expected
`status()` shows `chunking = recursive`. Each article produces one
refresh job; the worker splits it and fans out one embed job per chunk.
::::

## After any of the above

| Next | Where |
|---|---|
| Bind a column to a hosted API | [External providers](/docs/models/providers) |
| Restrict by metadata | [Filters](/docs/guides/filters) |
| Embed title + body together | [Templates](/docs/guides/templates) |
| Confirm the worker | [Status](/docs/guides/status) / `postvec doctor` |
| Re-drive failures | [`retry_dead()`](/docs/guides/retry) |
