---
title: Adopt existing vectors
description: Register a populated vector column without rewriting it.
---

# Adopt existing vectors

`adopt()` registers an application-owned `vector(N)` column and leaves
stored bytes as they are.

After adopt, search the column as it is. For a retired or provider-only
space such as ada-002, [search a retired space](/docs/guides/bridge)
([CLI](/docs/guides/bridge-cli)) converts each query into that space
(embed-bridge). Stored rows stay.
[`migrate()`](/docs/guides/migrate) is a later step, once that search is
working and you want a different stored model.

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

The column's declared dimension is fact. The model name is an
operator-supplied assertion. The catalogue checks that dimension against
the named model. Confirm the model that produced the bytes before
search or convert.

To re-attribute a column to a different space, disable without dropping
the vector column and adopt again with the new space name. Re-declare
`format` if you had one.

```sql
SELECT postvec.disable('public.legacy', 'body', drop_column => false);
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'openai-text-embedding-3-small'
);
```

Switching the route that produces vectors in the same space is
configuration (`model prefer`).

An incorrect assertion makes `search()` embed the query into an
incompatible space. `migrate(strategy => 'convert')` then converts
those bytes as if they belonged to the named model.

## Options

| Option | Default | Meaning |
|---|---|---|
| `sync` | `true` | Install enqueue triggers |
| `backfill` | `missing` | `missing` / `all` / `none` |
| `backfill_mode` | `queue` | `cursor` when the table holds more than 1,000,000 rows; `all` needs `queue` |
| `if_not_exists` | `false` | Return the existing id when the entry already has these options; a difference raises, as for [`enable()`](/docs/guides/enable) |
| `create_fts_index` | `false` | Build a GIN for the [BM25](/docs/guides/bm25) leg |
| `fts_config` | `pg_catalog.english` | Text-search configuration |
| others | same as `enable()` | distance, format, index_mode |

`sync` installs the write path; `backfill` performs the one-time pass over
existing rows. A `queue` backfill enqueues the eligible rows inside the calling
transaction. Above 1,000,000 existing rows `adopt()` refuses it and names
`cursor`, which the worker feeds in bounded chunks.

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

`trigger_mode` is stored as `none`. Only a TRUNCATE sentinel is
installed, so a dropped-and-recreated same-named table cannot silently
reattach. No embed route is required. Search may degrade to FTS.

`migrate()` on an observed entry requires
`observed_writes_quiesced => true`, and writes must stay stopped
**through `migration_finalize()`**. The flag records that
acknowledgement.

An observed entry can later be promoted by calling `adopt()` again with
`sync => true`. Stored immutable options, including `format`, must be
repeated **byte-exactly**.

## Requirements

The vector column must exist as exact `vector(N)` (typed dimension).
`halfvec`, arrays, domains, generated columns, PK members and aliases
of the source are out of scope. A `halfvec` error includes an
`ALTER TABLE ... TYPE vector(N) USING ...` recipe.

Synchronized adoption (`sync => true`) needs a nullable vector column:
the worker writes NULL when the source goes NULL. Observed mode
(`sync => false` and `backfill => 'none'`) accepts `NOT NULL`.

The named model must match the declared dimension. The column must be
unclaimed. `sync` or any finite backfill needs an embed route.
`backfill => 'all'` with `backfill_mode => 'cursor'` is refused.

## Teardown

`disable(drop_column => true)` and `uninstall(drop_columns => true)`
leave an adopted column in place.

:::: danger The source model must be known
An incorrect model assertion produces plausible but invalid ranks.
Columns with uncertain provenance should be rebuilt, or backfilled with
`backfill => 'all'` after selecting an available model.
::::

:::: danger Synchronized vector columns must permit NULL
A `NOT NULL` vector column is refused for synchronized adoption. Remove
the constraint first, or use observed mode with `backfill => 'none'`.
::::

## Where inference runs

Search and backfill embed through the inference host: the PostgreSQL process
by default. [postvec-server](/docs/server/) runs the same jobs in a separate
process. In remote mode the model inventory and the bridge chain for a retired
space are administered on the server ([models on
postvec-server](/docs/server/models)).
