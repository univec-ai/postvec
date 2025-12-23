---
title: Migrate models
description: In-place model migration via convert (or re-embed), finalize and abort.
---

# Migrate models

Stored vectors move to a new model in place. The default strategy is `convert`: UniVec translates the stored vectors.

When the corpus should remain unchanged, [bridge search](/docs/guides/bridge) converts queries into the existing space instead.

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

:::: tip Expected
After the first finalize, an entry that had an ANN index normally enters
`awaiting_index`. The column swap is already done; the entry is live on
the new model. Run the suggested concurrent index, then finalize again.
::::

```sql
SELECT suggested_index_sql
  FROM postvec.migration_status(:migration_id);
-- run that CREATE INDEX CONCURRENTLY in autocommit, then:
SELECT postvec.migration_finalize(:migration_id);
SELECT state FROM postvec.migration_status(:migration_id);
```

In psql, `\gexec` executes `suggested_index_sql`.

## Strategies

| `strategy` | What it does |
|---|---|
| `convert` (default) | Translate existing vectors. Needs a converter (or bridge) to the target. |
| `reembed` | Embed current source text (renders the template) with the new model. |
| `auto` | Convert when a route exists, otherwise re-embed. |

:::: code-group

```sql [convert]
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3',
  strategy => 'convert'
);
```

```sql [reembed]
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3',
  strategy => 'reembed'
);
```

```sql [auto]
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3',
  strategy => 'auto'
);
```

::::

`reindex`: `manual` (default) or `blocking`. Manual is recommended for tables serving application traffic.

On an embedded host, pull a converter with [`postvec model pull`](/docs/models/pull). On a remote host, install it on the ninference node.

## Abort

```sql
SELECT postvec.migration_abort(:migration_id);
```

`migration_abort()` is available **before** the swap. The original column remains unchanged.

## Observed entries

An `adopt(sync => false)` entry has no write path. `migrate()` refuses it unless `observed_writes_quiesced => true`, with writes remaining stopped through finalization. Otherwise the watermark can miss updates.

## Chunked entries

Counts refer to **chunks**. Conversion sends vectors. New writes during the migration embed with the new model into the scratch column.

## Finalization constraints

`migrate()` already refuses lossy column metadata (defaults, `NOT NULL`, constraints, comments, ACLs, stats/storage) before creating the `_new` scratch column. `finalize` takes `ACCESS EXCLUSIVE` and re-inspects. If dependent views or indexes appeared, it returns a retryable error without using `CASCADE`. The inspection walks `pg_partition_tree`.

## Validation constraints

:::: danger Conversion requires correct source-model provenance
An incorrect model assertion at `adopt()` produces invalid converted vectors.
Provenance must be confirmed first; otherwise use `strategy => 'reembed'`.
::::

:::: info `awaiting_index` means the column is already live
The data is on the new model. Build the index, then finalize again.
::::

:::: danger Only one migration may be active per entry
`migration_status()` reports the active migration. It can be aborted before
the column swap when a different target or strategy is required.
::::
