---
title: Adopt existing vectors
description: postvec.adopt() takes over a populated vector column without rewriting it.
---

# Adopt existing vectors

Use `adopt()` when the application already owns a `vector(N)` column.
postvec does **not** rewrite it during the call.

To **keep** that space (ada-002 and friends) and still search it, name
the original model and let embed-bridge produce query vectors —
[query an existing space](/docs/guides/bridge). To **leave** the space,
adopt then [`migrate()`](/docs/guides/migrate).

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'sentence-transformers-all-minilm-l6-v2'
);
```

::: tip Expected
`registry.owns_vector_column` is false. Only NULL vectors are queued
(`backfill => 'missing'`). Existing bytes are left alone. Future writes
are synchronized.
:::

## The model name is an assertion

The column's declared dimension is fact. The catalogue can only
*contradict* a wrong model (dimension mismatch). Nothing can prove which
model produced the bytes.

A wrong assertion makes `search()` embed the query into the wrong space
and `migrate(strategy => 'convert')` convert garbage — silently.

## Options

| Option | Default | Meaning |
|---|---|---|
| `sync` | `true` | Install enqueue triggers |
| `backfill` | `missing` | `missing` / `all` / `none` |
| `backfill_mode` | `queue` | `cursor` refused with `all` |
| others | same as `enable()` | distance, FTS, format, index_mode |

`sync` and `backfill` are independent. They are never rewritten from each
other.

## Observed (read-only) adoption

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'baai-bge-m3',
  sync => false,
  backfill => 'none'
);
```

`trigger_mode` is stored as `none`. Only a TRUNCATE sentinel is installed
(so a dropped-and-recreated same-named table cannot silently reattach).
No embed route is required. Search may degrade to FTS.

`migrate()` refuses an observed entry unless you pass
`observed_writes_quiesced => true`, and writes must stay stopped **through
`migration_finalize()`**. The flag is an acknowledgement, not a lock.

Promote later by calling `adopt()` again with `sync => true`. You must
repeat the stored immutable options **byte-exactly**, including `format`.

## What is refused

Missing column · not exact `vector` (`halfvec`, arrays, domains) · bare
`vector` with no dimension · generated / PK-member / alias of the source
· `NOT NULL` unless both `sync => false` and `backfill => 'none'` (the
worker must be allowed to NULL a vector when the source goes NULL) ·
dimension ≠ the model's known dimension · unknown model · column already
claimed · no embed route when `sync` or any finite backfill ·
`'all'` + `'cursor'`.

A `halfvec` refusal includes an `ALTER TABLE … TYPE vector(N) USING …`
recipe.

## Teardown never drops it

Because postvec did not create the column, `disable(drop_column => true)`
and `uninstall(drop_columns => true)` leave it in place.

## Don't

::: danger Don't guess the model to "just get search working"
Wrong space, confident ranks. If you are not sure, treat the column as
untrusted: `backfill => 'all'` after you pick a model you can actually
embed with, or rebuild.
:::

::: danger Don't adopt a `NOT NULL` vector you still want the worker to
write
Refused. Drop `NOT NULL` first, or use observed + `backfill => 'none'`.
:::
