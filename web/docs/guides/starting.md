---
title: Choose the SQL call
description: Match enable, adopt, bridge, migrate or chunking to the current table.
---

# Choose the SQL call

Column registration, search and migration are SQL functions. The CLI
configures the cluster and, in embedded mode, the model inventory.

Use the row that matches the table as it is now.

| Starting point | Call | Result |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | postvec creates and maintains a shadow `vector(N)` column |
| A populated `vector(N)` column, same model | [`adopt()`](/docs/guides/adopt) | existing bytes stay; missing rows can backfill |
| Vectors in a retired or provider-only space | [Bridge search](/docs/guides/bridge) | queries convert into that space; the corpus stays put |
| Ready to change model | [`migrate()`](/docs/guides/migrate) | stored vectors convert in place, or re-embed if that strategy is chosen |
| Long source documents | [Recursive chunking](/docs/guides/chunking) | a managed 1:N destination stores passage vectors; search returns documents |

Functions live in the `postvec` schema. Qualify every call:
`postvec.*`. The extension leaves `search_path` unchanged.

## Text, no vectors

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2',
  create_fts_index => true
);
```

Wait until `pending_jobs = 0`, then
[`create_vector_index()`](/docs/guides/indexes) and
[`search()`](/docs/guides/search).

## A populated vector column

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'sentence-transformers-all-minilm-l6-v2'
);
```

`adopt()` registers the existing column and leaves stored bytes as they
are. The `model` argument records provenance. A wrong name makes later
search and conversion silently invalid.

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

## Change the stored model

```sql
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3'
);
```

Default strategy is `convert`: UniVec translates the stored vectors.
Finalize after the worker reaches `awaiting_finalize`. See
[migrate](/docs/guides/migrate).

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

## After any of the above

| Next | Where |
|---|---|
| Restrict by metadata | [Filters](/docs/guides/filters) |
| Embed title + body together | [Templates](/docs/guides/templates) |
| Confirm the worker | [Status](/docs/guides/status) / `postvec doctor` |
| Re-drive failures | [`retry_dead()`](/docs/guides/retry) |
