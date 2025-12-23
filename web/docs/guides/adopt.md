---
title: Adopt existing vectors
description: Register a populated vector column without rewriting it.
---

# Adopt existing vectors

`adopt()` registers an application-owned `vector(N)` column and leaves stored bytes as they are.

To keep an existing space such as ada-002 and continue searching it, name the original model and let embed-bridge produce query vectors. See [search a retired space](/docs/guides/bridge). To change the space, adopt then [`migrate()`](/docs/guides/migrate).

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'sentence-transformers-all-minilm-l6-v2'
);
```

:::: tip Expected
`registry.owns_vector_column` is false. Only NULL vectors are queued
(`backfill => 'missing'`). Existing bytes are left alone. Future writes
are synchronized.
::::

## Model provenance

The column's declared dimension is fact. The model name is an operator-supplied assertion. The catalogue can only *contradict* an incorrect model (dimension mismatch). Nothing can prove which model produced the bytes.

An incorrect assertion makes `search()` embed the query into an incompatible space. `migrate(strategy => 'convert')` then produces invalid vectors without an explicit error.

## Options

| Option | Default | Meaning |
|---|---|---|
| `sync` | `true` | Install enqueue triggers |
| `backfill` | `missing` | `missing` / `all` / `none` |
| `backfill_mode` | `queue` | `cursor` refused with `all` |
| others | same as `enable()` | distance, FTS, format, index_mode |

`sync` and `backfill` are independent. Neither is derived from the other.

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

`trigger_mode` is stored as `none`. Only a TRUNCATE sentinel is installed, so a dropped-and-recreated same-named table cannot silently reattach. No embed route is required. Search may degrade to FTS.

`migrate()` on an observed entry requires `observed_writes_quiesced => true`, and writes must stay stopped **through `migration_finalize()`**. The flag records that acknowledgement.

An observed entry can later be promoted by calling `adopt()` again with `sync => true`. Stored immutable options, including `format`, must be repeated **byte-exactly**.

## Validation constraints

Missing column · not exact `vector` (`halfvec`, arrays, domains) · bare `vector` with no dimension · generated / PK-member / alias of the source · `NOT NULL` unless both `sync => false` and `backfill => 'none'` (the worker must be allowed to NULL a vector when the source goes NULL) · dimension != the model's known dimension · unknown model · column already claimed · no embed route when `sync` or any finite backfill · `'all'` + `'cursor'`.

A `halfvec` refusal includes an `ALTER TABLE ... TYPE vector(N) USING ...` recipe.

## Teardown behavior

Because postvec did not create the column, `disable(drop_column => true)` and `uninstall(drop_columns => true)` leave it in place.

## Provenance and write constraints

:::: danger The source model must be known
An incorrect model assertion produces plausible but invalid ranks. Columns
with uncertain provenance should be treated as untrusted and rebuilt, or
backfilled with `backfill => 'all'` after selecting an available model.
::::

:::: danger Synchronized vector columns must permit NULL
A `NOT NULL` vector column is refused for synchronized adoption. Remove the
constraint first, or use observed mode with `backfill => 'none'`.
::::
