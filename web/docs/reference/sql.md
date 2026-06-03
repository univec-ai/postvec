---
title: SQL reference
description: Function signatures, option values and grants.
outline: deep
---

# SQL reference

Signatures and grants. Walkthroughs live under [Usage](/docs/guides/).

`relation` and `fts_config` are `text`. The extension resolves them with
`to_regclass` / `::regconfig`, so `'schema.table'` works. `retry_dead()`
is the exception: it takes `regclass`.

Every call needs the `postvec` schema qualifier. `search_path` is left
as it is.

## Functions

| Function | Returns |
|---|---|
| `enable(relation, column_name, model, vector_column DEFAULT NULL, fts_config DEFAULT 'pg_catalog.english', create_fts_index DEFAULT false, backfill DEFAULT true, distance DEFAULT 'cosine', trigger_mode DEFAULT 'statement', index_mode DEFAULT 'manual', backfill_mode DEFAULT 'queue', format DEFAULT NULL, chunking DEFAULT 'none', chunk_size DEFAULT NULL, chunk_overlap DEFAULT NULL, destination DEFAULT NULL)` | `bigint` registry id |
| `adopt(relation, column_name, vector_column, model, sync DEFAULT true, backfill DEFAULT 'missing', backfill_mode DEFAULT 'queue', distance DEFAULT 'cosine', trigger_mode DEFAULT 'statement', fts_config DEFAULT 'pg_catalog.english', create_fts_index DEFAULT false, format DEFAULT NULL, index_mode DEFAULT 'manual')` | `bigint` registry id |
| `set_format(relation, column_name, format)` | `void` |
| `disable(relation, column_name, drop_column DEFAULT false, drop_destination DEFAULT false)` | `void` |
| `uninstall(drop_columns DEFAULT false, drop_destinations DEFAULT false)` | `bigint` entries torn down; superuser |
| `create_vector_index(relation, column_name)` | `void` |
| `search(relation, column_name, query, limit_n DEFAULT 10, semantic_weight DEFAULT 0.5, rrf_k DEFAULT 60, candidates DEFAULT NULL, filter DEFAULT NULL)` | `TABLE(pk_value text, rrf_score float8, semantic_rank bigint, fts_rank bigint, chunk_seq int, chunk_start bigint, chunk_end bigint, chunk_text text)` |
| `search_with_vector(relation, column_name, query_vector real[], query_text DEFAULT '', ...same including filter...)` | same |
| `retry_dead(relation regclass, column_name, dead_ids bigint[] DEFAULT NULL)` | `bigint` dead rows consumed |
| `migrate(relation, column_name, new_model, strategy DEFAULT 'convert', reindex DEFAULT 'manual', observed_writes_quiesced DEFAULT false)` | `bigint` migration id |
| `migration_status(migration_id DEFAULT NULL)` | route in `resolved_via`, progress, state and `suggested_index_sql` |
| `migration_finalize(id)` / `migration_abort(id)` | `void` |
| `status()` | per-entry health (includes chunk + index columns) |
| `stats()` | worker / queue counters |
| `embed(input text, model text)` / `embed(inputs text[], model text)` | `real[]` / `setof real[]` |
| `convert(embedding real[], source_model text, target_model text)` | `real[]` |
| `refresh_models()` | `int` |
| `version()` / `build_info()` | `text` / `jsonb` |

Guides: [enable](/docs/guides/enable) · [adopt](/docs/guides/adopt) ·
[search](/docs/guides/search) · [filters](/docs/guides/filters) ·
[templates](/docs/guides/templates) · [chunking](/docs/guides/chunking) ·
[migrate](/docs/guides/migrate) · [indexes](/docs/guides/indexes) ·
[retry](/docs/guides/retry).

## Option values

| Option | Values |
|---|---|
| `distance` | `cosine`, `l2`, `ip` |
| `trigger_mode` | `statement` (default), `row`; stored `none` for observed adopt |
| `backfill_mode` | `queue` (default), `cursor` |
| `adopt.backfill` | `missing` (default), `all`, `none` |
| `index_mode` | `manual` (default), `immediate`, `auto` |
| `migrate.strategy` | `convert` (default), `reembed`, `auto` |
| `migrate.reindex` | `manual` (default), `blocking` |
| `chunking` | `none` (default), `recursive` |

## Grants

Untrusted cdylib. `CREATE EXTENSION` needs superuser.

| Who | What |
|---|---|
| Table owner (or superuser) | `enable`, `adopt`, `disable`, `set_format`, `migrate`, `create_vector_index`, `retry_dead` |
| Superuser | `uninstall()` |
| PUBLIC | `search`, `search_with_vector`, `status`, `stats`, `version`, `build_info` |
| Revoked from PUBLIC | `embed`, `convert`, `refresh_models` |

Non-owner DML on an enabled table needs `USAGE` on schema `postvec` and a
column-scoped `INSERT (registry_id, pk_value)` on `postvec.jobs`. Those
grants must stay.

The worker connects as the bootstrap superuser and **bypasses RLS**.

## `enable()` refusals

Refused: no primary key, a relation that is not ordinary or partitioned,
`TEMPORARY`, an unknown model or a model with no embed route. Unlogged
tables are accepted with a warning. Composite primary keys work in
column mode (compared as text; `DateStyle` / `TimeZone` must stay consistent)
and are refused for chunking.
