---
title: Migrate models
description: In-place model migration via convert (or re-embed), finalize, and abort.
---

# Migrate models

Stored vectors move to a new model in place. Default strategy is
`convert`: UniVec translates the vectors. Source text is not
re-embedded.

If you do not want to move the corpus, [bridge the query](/docs/guides/bridge)
into the existing space instead.

```sql
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3'
) AS migration_id \gset

SELECT migration_id, state, rows_done, rows_total, progress_pct, error
  FROM postvec.migration_status(:migration_id);
```

Repeat the status query until `awaiting_finalize`, then:

```sql
SELECT postvec.migration_finalize(:migration_id);
```

::: tip Expected
After the first finalize, an entry that had an ANN index normally enters
`awaiting_index`. The column swap is already done; the entry is live on
the new model. Run the suggested concurrent index, then finalize again.
:::

```sql
SELECT suggested_index_sql
  FROM postvec.migration_status(:migration_id);
-- run that CREATE INDEX CONCURRENTLY in autocommit, then:
SELECT postvec.migration_finalize(:migration_id);
SELECT state FROM postvec.migration_status(:migration_id);
```

`\gexec` in psql will run `suggested_index_sql` for you.

## Strategies

| `strategy` | What it does |
|---|---|
| `convert` (default) | Translate existing vectors. Needs a converter (or bridge) to the target. |
| `reembed` | Embed current source text (renders the template) with the new model. |
| `auto` | Convert when a route exists, otherwise re-embed. |

`reindex`: `manual` (default) or `blocking`. Manual is the right default
on a table that already has traffic.

Need a converter on an embedded host? [`postvec model pull`](/docs/models/pull).
Need one on a remote host? Install it on the ninference node.

## Abort

```sql
SELECT postvec.migration_abort(:migration_id);
```

Available **before** the swap. The original column is untouched.

## Observed entries

An `adopt(sync => false)` entry has no write path. `migrate()` refuses it
unless `observed_writes_quiesced => true`, and you keep writes stopped
through finalize. Otherwise the watermark can miss updates behind it.

## Chunked entries

Counts are **chunk** counts. Convert sends vectors, never chunk text.
New writes during the migration embed with the new model into the
scratch column.

## What `finalize` will refuse

`migrate()` already refuses lossy column metadata (defaults, `NOT NULL`,
constraints, comments, ACLs, stats/storage) before creating the `_new`
scratch column. `finalize` takes `ACCESS EXCLUSIVE` and re-inspects —
retryably, never `CASCADE` — if dependent views/indexes appeared. The
inspection walks `pg_partition_tree`.

## Don't

::: danger Don't `migrate()` to a model you asserted wrongly at `adopt()`
Convert will faithfully map garbage into the new space. Confirm
provenance first, or use `strategy => 'reembed'`.
:::

::: danger Don't treat `awaiting_index` as failure
The data is already on the new model. Build the index, finalize again.
:::

::: danger Don't start a second migration on the same entry
One live migration per entry. Watch `migration_status()`; abort if you
need to change your mind before the swap.
:::
