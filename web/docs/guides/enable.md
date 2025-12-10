---
title: Enable a column
description: Behavior and options for postvec.enable().
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

The function returns the registry identifier.

If the table already has a populated `vector(N)` column, use
[`adopt()`](/docs/guides/adopt) instead. `enable()` will not take it over.

## Requirements

- The table is ordinary or partitioned, not `TEMPORARY`.
- It has a primary key.
- The model exists in `postvec.models` and has an embed route.
- The invoking role owns the table or is a superuser.

```sql
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
```

Unlogged tables are allowed with a durability warning.

## Useful options

| Option | Default | Change when |
|---|---|---|
| `vector_column` | `{col}_semantic` | A specific column name is required |
| `create_fts_index` | `false` | Hybrid search should have a GIN |
| `fts_config` | `pg_catalog.english` | Non-English lexical |
| `distance` | `cosine` | Search and index operations use another distance metric |
| `trigger_mode` | `statement` | Use `row` on partitioned tables |
| `index_mode` | `manual` | `auto` only on small, quiet tables |
| `backfill` | `true` | `false` when the initial data load follows configuration |
| `backfill_mode` | `queue` | `cursor` for a huge existing table |
| `format` | raw column | See [templates](/docs/guides/templates) |
| `chunking` | `none` | See [chunking](/docs/guides/chunking) |

Statement triggers do **not** fire for subscriber-applied logical
replication. Run postvec on the **publisher**. On a partitioned parent,
prefer `trigger_mode => 'row'` so every partition is covered.

## Verification after `enable()`

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

With `index_mode => 'manual'`, `doctor` reports the missing ANN index as a
warning rather than a failure. The index can be built after backfill completes;
see [indexes](/docs/guides/indexes).

## Disable

```sql
SELECT postvec.disable('public.docs', 'body');
-- drop the shadow column postvec created:
SELECT postvec.disable('public.docs', 'body', drop_column => true);
```

`drop_column` is refused for a column postvec did not create (adopted).
Chunk destinations are a separate flag — [chunking](/docs/guides/chunking).

## Validation constraints

::: danger A primary key is required
Jobs are keyed by primary key, so tables without one are refused.
:::

::: danger Temporary tables are unsupported
The worker uses a separate session and cannot access temporary tables.
:::

::: danger Source text is visible to the bootstrap superuser
The worker connects as the bootstrap superuser and bypasses RLS. Anyone
who can read the table can read the derived vector.
:::
