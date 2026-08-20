---
title: Enable a column
description: Behavior and options for postvec.enable().
---

# Enable a column

`enable()` declares a text column semantic. The call **creates** a shadow
`vector(N)` column (default name `{column}_semantic`), installs enqueue
triggers and optionally backfills existing rows.

If the table already has a populated `vector(N)` column, use
[`adopt()`](/docs/guides/adopt). For a retired or provider-only space,
[search the existing space](/docs/guides/bridge) after adopt. `enable()`
always adds a new column.

## 1. Check the table and the model

- The table is ordinary or partitioned, not `TEMPORARY`.
- It has a primary key.
- The invoking role owns the table or is a superuser.
- The model exists in `postvec.models` and has an embed route.

```sql
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
```

Unlogged tables are accepted and emit a durability warning.

## 2. Call `enable()`

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2'
);
```

The function returns the registry identifier. A
[provider-backed](/docs/models/providers) model also emits a NOTICE
naming the provider and the column, because source text will leave the
host.

## 3. Wait for the worker

```sql
INSERT INTO docs (body) VALUES ('...');

SELECT relation, model, dim, pending_jobs, dead_jobs
  FROM postvec.status();
```

:::: info Optional
`sudo postvec doctor --database app --deep` is the host-side check.
[Verify artifacts](/docs/install/verify).
::::

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
reason search is slow. See [indexes](/docs/guides/indexes).

## Options

| Option | Default | Change when |
|---|---|---|
| `vector_column` | `{col}_semantic` | A specific column name is required |
| `create_fts_index` | `false` | Hybrid search should have a GIN |
| `fts_config` | `pg_catalog.english` | Non-English lexical |
| `distance` | `cosine` | Search and index use another metric |
| `trigger_mode` | `statement` | Use `row` on partitioned tables |
| `index_mode` | `manual` | `auto` only on small, quiet tables |
| `backfill` | `true` | `false` when the initial load follows configuration |
| `backfill_mode` | `queue` | `cursor` for a huge existing table |
| `format` | raw column | See [templates](/docs/guides/templates) |
| `chunking` | `none` | See [chunking](/docs/guides/chunking) |

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

`drop_column` applies only to a shadow column postvec created. Adopted
columns stay. Chunk destinations are a separate flag; see
[chunking](/docs/guides/chunking).

## Refusals

:::: danger A primary key is required
Jobs are keyed by primary key. Tables without one are refused.
::::

:::: danger Temporary tables are unsupported
The worker uses a separate session and cannot see temporary tables.
::::

:::: danger Source text is visible to the worker
The worker connects as a superuser and bypasses RLS. A role that can
read the table can also read the derived vector.
::::
