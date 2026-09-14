---
title: SQL functions
description: The postvec call surface, grouped by task, with the grants each call needs.
---

# SQL functions

Functions live in the `postvec` schema, so every call is qualified:
`postvec.enable(...)`, `postvec.search(...)`. Signatures, option values and
defaults are in the [SQL reference](/docs/reference/sql).

## Where to start

| Situation | First call | Then |
|---|---|---|
| A text column with no vectors | [`enable()`](/docs/guides/enable) | [index](/docs/guides/indexes), then [search](/docs/guides/search) |
| A populated `vector(N)` column | [`adopt()`](/docs/guides/adopt) | [search](/docs/guides/search) on the stored space |
| Vectors from a model you no longer call | [`adopt()`](/docs/guides/adopt) with that model | [search a retired space](/docs/guides/bridge): each query is converted into the stored space |
| Long source documents | [`enable(chunking => 'recursive')`](/docs/guides/chunking) | search returns one row per document plus the winning chunk |
| A stored model to replace | [`migrate()`](/docs/guides/migrate) | [`migration_finalize()`](/docs/guides/migrate) swaps the column |
| An existing pgai or pg_vectorize pipeline | [Coming from pgai](/docs/from-pgai) | map the vectorizer to `enable()` or `adopt()` |

## Lifecycle

| Call | Effect |
|---|---|
| [`enable(relation, column_name, model, ...)`](/docs/guides/enable) | Adds the shadow vector column, installs enqueue triggers and queues the existing rows |
| [`adopt(relation, column_name, vector_column, model, ...)`](/docs/guides/adopt) | Registers a populated vector column and leaves the stored bytes alone |
| [`set_format(relation, column_name, format)`](/docs/guides/templates) | Replaces the embedding template and refreshes the entry in one transaction |
| [`disable(relation, column_name, ...)`](/docs/install/uninstall) | Removes the objects the entry owns and marks its registry row `state = 'disabled'` |
| [`uninstall(...)`](/docs/install/uninstall) | Tears down every entry and marks each registry row `state = 'disabled'`; tables and vectors stay unless the destructive flags are set |

A disabled row stays in `postvec.registry`. A later `enable()` or `adopt()` on
that column clears it and registers a new row.

## Search

| Call | Effect |
|---|---|
| [`search(relation, column_name, query, ...)`](/docs/guides/search) | Hybrid search in one call. Full-text and vector results fused with reciprocal rank fusion |
| [`search_with_vector(relation, column_name, query_vector, query_text, ...)`](/docs/guides/search) | The same search when the application already holds the query vector |
| [`create_vector_index(relation, column_name)`](/docs/guides/indexes) | Builds the ANN index for the entry. `index_mode` can also ask the worker to build it |
| [`refresh_lexical_stats(relation, column_name)`](/docs/guides/bm25) | Rebuilds the BM25 corpus statistics now |

Filters (`filter =>`), the semantic/lexical mix (`semantic_weight`), the candidate
pool (`candidates`) and chunk collapse are documented in
[Search](/docs/guides/search), [Filters](/docs/guides/filters) and
[BM25](/docs/guides/bm25).

## Migration

| Call | Effect |
|---|---|
| [`migrate(relation, column_name, new_model, strategy => ...)`](/docs/guides/migrate) | Starts a migration: `convert` translates stored vectors, `reembed` re-runs the source text |
| `migration_status(migration_id)` | State, progress, the route in use and the index statement to run next |
| `migration_finalize(migration_id)` | Swaps in the migrated column; a fresh ANN index finishes in a second call |
| `migration_abort(migration_id)` | Stops the migration and keeps the original column |
| `convert(embedding, source_model, target_model)` | One-shot vector conversion, callable on its own |

## Operations

| Call | Effect |
|---|---|
| [`status()`](/docs/guides/status) | Per-entry health: queue depth, dead jobs, readiness, chunking, index and lexical state |
| [`stats()`](/docs/guides/status) | Worker and queue counters |
| [`retry_dead(relation, column_name, dead_ids)`](/docs/guides/retry) | Re-drives dead-lettered rows as fresh deduplicated jobs |
| `refresh_models()` | Re-reads the model inventory from the inference host |
| `embed(input, model)` | One-shot embedding, outside the queue |
| `version()` / `build_info()` | Extension version and build details |

## Grants

`enable`, `adopt`, `disable`, `set_format`, `migrate`, `create_vector_index`,
`retry_dead` and `refresh_lexical_stats` need the table owner or a superuser.
`uninstall()` and `start_worker()` are superuser only. `search()`,
`search_with_vector()`, `status()`, `stats()` and `version()` are executable by
PUBLIC. `embed`, `convert` and `refresh_models` are revoked from PUBLIC.

Non-owner DML on an enabled table needs `USAGE` on schema `postvec` and a
column-scoped `INSERT (registry_id, pk_value)` on `postvec.jobs`.

## Where inference runs

Inference runs inside the PostgreSQL process by default. The SQL above is the
same when the work runs in [postvec-server](/docs/server/): a separate process,
multi-threaded on the same VM, or a CPU and GPU fleet on the network. A database
that cannot load the extension uses the server for
[managed PostgreSQL](/docs/server/managed).

- [Enable](/docs/guides/enable) - [Adopt](/docs/guides/adopt) - [Search](/docs/guides/search)
- [Chunking](/docs/guides/chunking) - [Migration](/docs/guides/migrate) - [Indexes](/docs/guides/indexes)
- [Status](/docs/guides/status) - [Retry](/docs/guides/retry) - [Backup](/docs/guides/backup)
- [SQL reference](/docs/reference/sql) - [GUCs](/docs/reference/gucs) - [CLI](/docs/reference/cli)
