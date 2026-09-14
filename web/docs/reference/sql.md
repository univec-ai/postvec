---
title: SQL reference
description: Function signatures, option values and grants.
outline: deep
---

# SQL reference

Every call is `postvec.<name>(...)`; usage examples live in the
[SQL functions](/docs/guides/) guides.

`relation` and `fts_config` are `text`. The extension resolves them with
`to_regclass` / `::regconfig`, so `'schema.table'` works. `retry_dead()` and
`refresh_lexical_stats()` declare `relation` as `regclass`, so the invoking
role's `search_path` resolves the name before the function body runs.

## Functions

| Function | Returns |
|---|---|
| `enable(relation, column_name, model, vector_column DEFAULT NULL, fts_config DEFAULT 'pg_catalog.english', create_fts_index DEFAULT false, backfill DEFAULT true, distance DEFAULT 'cosine', trigger_mode DEFAULT 'statement', index_mode DEFAULT 'manual', backfill_mode DEFAULT 'queue', format DEFAULT NULL, chunking DEFAULT 'none', chunk_size DEFAULT NULL, chunk_overlap DEFAULT NULL, destination DEFAULT NULL, if_not_exists DEFAULT false)` | `bigint` registry id |
| `adopt(relation, column_name, vector_column, model, sync DEFAULT true, backfill DEFAULT 'missing', backfill_mode DEFAULT 'queue', distance DEFAULT 'cosine', trigger_mode DEFAULT 'statement', fts_config DEFAULT 'pg_catalog.english', create_fts_index DEFAULT false, format DEFAULT NULL, index_mode DEFAULT 'manual', if_not_exists DEFAULT false)` | `bigint` registry id |
| `set_format(relation, column_name, format)` | `void` |
| `disable(relation, column_name, drop_column DEFAULT false, drop_destination DEFAULT false)` | `void` |
| `uninstall(drop_columns DEFAULT false, drop_destinations DEFAULT false)` | `bigint` entries torn down; superuser |
| `create_vector_index(relation, column_name)` | `void` |
| `refresh_lexical_stats(relation regclass, column_name)` | `void`; table owner. Rebuilds BM25 corpus stats now; raises if the rebuild fails |
| `search(relation, column_name, query, limit_n DEFAULT 10, semantic_weight DEFAULT 0.5, rrf_k DEFAULT 60, candidates DEFAULT NULL, filter DEFAULT NULL)` | `TABLE(pk_value text, rrf_score float8, semantic_rank bigint, fts_rank bigint, semantic_distance float8, fts_score float8, chunk_seq int, chunk_start bigint, chunk_end bigint, chunk_text text)` |
| `search_with_vector(relation, column_name, query_vector real[], query_text DEFAULT '', ...same including filter...)` | same |
| `retry_dead(relation regclass, column_name, dead_ids bigint[] DEFAULT NULL)` | `bigint` dead rows consumed |
| `migrate(relation, column_name, new_model, strategy DEFAULT 'convert', reindex DEFAULT 'manual', observed_writes_quiesced DEFAULT false)` | `bigint` migration id |
| `migration_status(migration_id DEFAULT NULL)` | route in `resolved_via`, progress, state and `suggested_index_sql` |
| `migration_finalize(id)` / `migration_abort(id)` | `void` |
| `status()` | per-entry health, led by `worker_alive` (includes chunk + index columns, `lexical_docs`, `lexical_stats_age_seconds`, `lexical_error`, `space`, `route`, `route_execution`) |
| `postvec.routes` | view over embed rows: `space`, `route`, `provider`, `execution`, `dim`, `priority`, `explicit`, `preferred`, `last_seen` |
| `postvec._route(model, space DEFAULT NULL)` | the served route a bound string resolves to: the exact route named, else that space's routes by priority, else `space` (an entry's remembered space); ties on route name. Every resolver, the worker and `status()` use it |
| `stats()` | worker / queue counters |
| `embed(input text, model text)` / `embed(inputs text[], model text)` | `real[]` / `setof real[]` |
| `convert(embedding real[], source_model text, target_model text)` | `real[]` |
| `refresh_models()` | `int` |
| `start_worker()` | `bool`; superuser. Starts this database's worker without `shared_preload_libraries`; false if one is running. A crash restarts it; a server restart re-creates it only through the preload path |
| `version()` / `build_info()` | `text` / `jsonb` |

Guides: [enable](/docs/guides/enable), [adopt](/docs/guides/adopt),
[search](/docs/guides/search), [BM25](/docs/guides/bm25),
[filters](/docs/guides/filters), [templates](/docs/guides/templates),
[chunking](/docs/guides/chunking), [migrate](/docs/guides/migrate),
[indexes](/docs/guides/indexes), [retry](/docs/guides/retry).

## Option values

| Option | Values |
|---|---|
| `distance` | `cosine`, `l2`, `ip` |
| `trigger_mode` | `statement` (default), `row`; stored `none` for observed adopt |
| `backfill_mode` | `queue` (default), `cursor`; a `queue` backfill over a table holding more than 1,000,000 rows is refused, leaving the queue empty |
| `adopt.backfill` | `missing` (default), `all`, `none` |
| `index_mode` | `manual` (default), `immediate`, `auto` |
| `migrate.strategy` | `convert` (default), `reembed`, `auto` |
| `migrate.reindex` | `manual` (default), `blocking` |
| `chunking` | `none` (default), `recursive` |
| `create_fts_index` | `false` (default), `true` - GIN for the [BM25](/docs/guides/bm25) path |
| `fts_config` | a `regconfig`, default `pg_catalog.english` |

## Grants

The extension is an untrusted cdylib; `CREATE EXTENSION` needs superuser.

| Who | What |
|---|---|
| Table owner (or superuser) | `enable`, `adopt`, `disable`, `set_format`, `migrate`, `create_vector_index`, `retry_dead`, `refresh_lexical_stats` |
| Superuser | `uninstall()` |
| PUBLIC | `search`, `search_with_vector`, `status`, `stats`, `version`, `build_info` |
| Revoked from PUBLIC | `embed`, `convert`, `refresh_models` |
| Superuser only | `uninstall`, `start_worker` |

Non-owner DML on an enabled table needs `USAGE` on schema `postvec` and a
column-scoped `INSERT (registry_id, pk_value)` on `postvec.jobs`. Those
grants must stay.

The worker connects as the bootstrap superuser and **bypasses RLS**.

## Managed PostgreSQL

The [managed schema](/docs/server/managed) is plain SQL and uses the same
function names. Four calls behave differently:

- `search(relation, column_name, query, ...)` is a stub that raises, unless the
  call arrives through the postvec-server proxy port. The proxy rewrites the
  text query into `search_with_vector()`. A direct connection raises and names
  the supported call forms in the HINT.
- `embed(input, model)` is a stub for the same reason; the HINT names the
  server's `/api/openai/embeddings` endpoint. Only the single-text form exists
  on a managed host.
- `convert()` raises. POST `{"source_model","target_model","embeddings"}` to
  the server's `/api/convert` endpoint.
- `refresh_models()` returns `void` and notifies the worker, which refreshes the
  cache.

`search_with_vector()`, the lifecycle verbs, `stats()` and `status()` run as
printed above, except that `worker_alive` means a beat less than 30 seconds
old.

A managed host has no GUCs and no `postvec setup`. Host-side settings come from
the `managed[]` block of the postvec-server config file; `postvec.settings` holds
the detected platform and the worker's runtime markers.

## `enable()` requirements

The table must be ordinary or partitioned, have a primary key and name
a model that has an embed route. Unlogged tables are accepted with a
warning. Composite primary keys work in column mode (compared as text;
`DateStyle` / `TimeZone` must stay consistent). Chunking needs a
single-column PK.
