---
title: Enable a column
description: postvec.enable() — create a shadow vector column and start sync.
---

# Enable a column

`enable()` declares a text column semantic. It **creates** a shadow
`vector(N)` column (default name `{column}_semantic`), installs enqueue
triggers, and optionally backfills.

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2'
);
```

Returns the registry id.

If the table already has a populated `vector(N)` column, use
[`adopt()`](/docs/guides/adopt) instead. `enable()` will not take it over.

## Before you call it

- The table is ordinary or partitioned, not `TEMPORARY`.
- It has a primary key.
- The model exists in `postvec.models` and has an embed route.
- You own the table (or you are superuser).

```sql
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
```

Unlogged tables are allowed with a durability warning.

## Useful options

| Option | Default | When to change it |
|---|---|---|
| `vector_column` | `{col}_semantic` | You want a specific name |
| `create_fts_index` | `false` | Hybrid search should have a GIN |
| `fts_config` | `pg_catalog.english` | Non-English lexical |
| `distance` | `cosine` | Must match how you will search / index |
| `trigger_mode` | `statement` | Use `row` on partitioned tables |
| `index_mode` | `manual` | `auto` only on small, quiet tables |
| `backfill` | `true` | `false` if you will load data next |
| `backfill_mode` | `queue` | `cursor` for a huge existing table |
| `format` | raw column | See [templates](/docs/guides/templates) |
| `chunking` | `none` | See [chunking](/docs/guides/chunking) |

Statement triggers do **not** fire for subscriber-applied logical
replication. Run postvec on the **publisher**. On a partitioned parent,
prefer `trigger_mode => 'row'` so every partition is covered.

## After `enable()`

```sql
INSERT INTO docs (body) VALUES ('…');

SELECT relation, model, dim, pending_jobs, dead_jobs
  FROM postvec.status();
```

::: tip Expected
`pending_jobs` rises, then returns to 0. `docs.body_semantic` fills with
`vector(N)` where `N` is the model's `target_dim`.
:::

CLI check:

```bash
sudo postvec doctor --database app --deep
```

`doctor` will warn that a `manual` entry has no ANN index. That is the
default, not a failure. Build one when backfill is done —
[indexes](/docs/guides/indexes).

## Disable

```sql
SELECT postvec.disable('public.docs', 'body');
-- drop the shadow column postvec created:
SELECT postvec.disable('public.docs', 'body', drop_column => true);
```

`drop_column` is refused for a column postvec did not create (adopted).
Chunk destinations are a separate flag — [chunking](/docs/guides/chunking).

## Don't

::: danger Don't enable a table without a primary key
Refused. Jobs are keyed by PK.
:::

::: danger Don't enable a TEMP table
The worker has its own session and will never see it.
:::

::: danger Don't `enable()` a column whose text must be hidden from superuser
The worker connects as the bootstrap superuser and bypasses RLS. Anyone
who can read the table can read the derived vector.
:::
