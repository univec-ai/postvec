---
title: Enable a column (SQL)
description: postvec.enable() options, disable and refusals.
---

# Enable a column

`enable()` declares a text column semantic. The call **creates** a shadow
`vector(N)` column (default name `{column}_semantic`), installs enqueue
triggers and optionally backfills existing rows.

If the table already has a populated `vector(N)` column, use
[`adopt()`](/docs/guides/adopt). For a retired or provider-only space,
[search the existing space](/docs/guides/bridge) after adopt.

## 1. Check the table and the model

- The table is ordinary or partitioned (durable).
- It has a primary key.
- The invoking role owns the table or is a superuser.
- The model exists in `postvec.models` and has an embed route.

```sql
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
```

In remote mode those names come from [postvec-server](/docs/server/models).
Unlogged tables are accepted and emit a durability warning.

## 2. Call `enable()`

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2'
);
```

The function returns the registry identifier. A
[provider-backed](/docs/models/providers) model (OpenAI, Cohere, Bedrock,
Gemini, Mistral, OpenRouter or UniVec) also emits a NOTICE naming the
provider and the column, because source text will leave the host.

## 3. Wait for the worker

```sql
INSERT INTO docs (body) VALUES ('...');

SELECT relation, model, dim, pending_jobs, dead_jobs
  FROM postvec.status();
```

Host-side check: [enable (CLI)](/docs/guides/enable-cli).

:::: tip Expected
`pending_jobs` rises, then returns to 0. `docs.body_semantic` fills with
`vector(N)` where `N` is the model's `target_dim`. With
`index_mode => 'manual'`, `has_vector_index` stays false until
`create_vector_index()`.
::::

Vectors fill after the inserting transaction commits. Poll `status()` in
a later transaction. See [eventual consistency](/docs/concepts/consistency).

## 4. Index, then search

```sql
SELECT postvec.create_vector_index('public.docs', 'body');
```

Then [`search()`](/docs/guides/search). A missing ANN index is the usual
reason search is slow. See [indexes](/docs/guides/indexes). The keyword
leg, GIN and corpus stats: [BM25](/docs/guides/bm25).

## Options

| Option | Default | Change when |
|---|---|---|
| `vector_column` | `{col}_semantic` | A specific column name is required |
| `create_fts_index` | `false` | Build a GIN for the [BM25](/docs/guides/bm25) leg |
| `fts_config` | `pg_catalog.english` | Text-search configuration (stemming, stopwords) |
| `distance` | `cosine` | Search and index use another metric |
| `trigger_mode` | `statement` | Use `row` on partitioned tables |
| `index_mode` | `manual` | `auto` only on small, quiet tables |
| `backfill` | `true` | `false` when the initial load follows configuration |
| `backfill_mode` | `queue` | `cursor` when the table holds more than 1,000,000 rows |
| `format` | raw column | See [templates](/docs/guides/templates) |
| `chunking` | `none` | See [chunking](/docs/guides/chunking) |
| `if_not_exists` | `false` | Return the existing id when the entry already has these options; scripts that rerun |

A `queue` backfill enqueues every eligible row inside the calling transaction,
with the relation lock held. Above 1,000,000 existing rows `enable()` refuses it
and names `cursor`; the worker then feeds the backfill in bounded chunks.

`if_not_exists` compares `model`, `vector_column`, `distance`, `trigger_mode`,
`index_mode`, `fts_config`, `format` and the chunking settings with the existing
entry. When they match it returns the existing id; a difference raises and
names it. `backfill`, `backfill_mode` and `create_fts_index` act once, at
creation, and are not compared. Reconfigure with `disable()` then `enable()`,
or change a template with `set_format()`.

Statement triggers fire on the publisher. Subscriber-applied logical
replication skips them, so run postvec on the **publisher**. On a
partitioned parent, prefer `trigger_mode => 'row'` so every partition
is covered.

## Disable

```sql
SELECT postvec.disable('public.docs', 'body');
-- drop the shadow column postvec created:
SELECT postvec.disable('public.docs', 'body', drop_column => true);
```

`drop_column` applies only to a shadow column postvec created; an adopted
column stays. Chunk destinations are a separate flag; see
[chunking](/docs/guides/chunking).

## Requirements

:::: danger Primary key
Jobs are keyed by primary key. The table must have one.
::::

:::: danger Durable tables
The worker runs in its own session. The source must be an ordinary or
partitioned table, visible to that session.
::::

:::: danger Source text is visible to the worker
The worker connects as a superuser and bypasses RLS. A role that can
read the table can also read the derived vector.
::::

- [Enable (CLI)](/docs/guides/enable-cli)
- [Search](/docs/guides/search)
- [Status](/docs/guides/status)
