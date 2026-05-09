---
title: Search a retired space
description: Adopt vectors in a retired or provider-only model space and search them through local embed-bridge routing.
---

# Search a retired space

If the column is already in a retired or provider-only space, postvec
can create each new query vector locally and convert that vector into
the stored space with a local bridge chain.

An `openai-text-embedding-ada-002` corpus can stay byte-for-byte
unchanged. `search()` embeds the query with an available open model,
converts that one vector to ada-002 space, then runs pgvector and
full-text search against the existing table.

`search()` selects the bridge route. There is no extra function.

## Route resolution

```text
query text
  -> direct local embed model
      -> converter into the stored model space
          -> existing vector column
```

Resolution is deterministic:

1. A direct embed model for the declared target takes precedence.
2. Otherwise postvec chooses the lexicographically first local converter
   into that target whose source is directly embeddable, then uses an
   `embed-bridge` executor.
3. If neither route exists, `search()` follows the configured
   FTS-degradation policy. Administrative and write paths fail or retry
   normally.

If a direct embed route becomes available later, it takes precedence
after the next model refresh. The resolved route is inventory state.
postvec does not persist it in the table registry.

:::: warning A provider key ends the bridge
Configuring an [external provider](/docs/models/providers) for the stored
space makes a direct route exist. The column then stops bridging and starts
sending its source text to that provider, with no SQL change. `postvec
provider add` lists the affected columns and requires an acknowledgement
before it writes the file.
::::

:::: info Hosted converters do not supply a bridge
A [UniVec hosted converter](/docs/models/univec) can serve a direct
`migrate()` or `convert()` route. It cannot back writes or query embedding,
and it cannot act as the converter in an `embed-bridge` route. Embed-bridge
resolution runs inside the local engine.
::::

## 1. Verify the stored space

The vector dimension is a fact. The model name is an operator-supplied
provenance assertion. Check both before adoption:

```sql
SELECT vector_dims(embedding) AS stored_dim
  FROM public.legacy
 WHERE embedding IS NOT NULL
 LIMIT 1;

SELECT name, model_type, source_model, target_model, target_dim
  FROM postvec.models
 WHERE name = 'openai-text-embedding-ada-002'
    OR target_model = 'openai-text-embedding-ada-002'
 ORDER BY model_type, name;
```

:::: warning Provenance cannot be inferred from the bytes
Matching `vector(1536)` only rules out models with another dimension.
It does not prove that ada-002 produced the vectors. An incorrect
model assertion yields plausible but invalid ranks.
::::

## 2. Adopt without rewriting

For a live table whose future writes should stay synchronized:

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'openai-text-embedding-ada-002',
  sync => true,
  backfill => 'none'
);
```

`backfill => 'none'` leaves existing rows unchanged. `sync => true`
writes future `INSERT` and `UPDATE` vectors in the same target space
through the same bridge route.

For a frozen or `NOT NULL` legacy vector column, observe it without a
write path:

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'openai-text-embedding-ada-002',
  sync => false,
  backfill => 'none'
);
```

Observed mode can search but does not synchronize future rows. See
[`adopt()`](/docs/guides/adopt) before migrating an observed entry.

## 3. Search the adopted column

```sql
SELECT d.id, d.body,
       round(s.rrf_score::numeric, 5) AS score,
       s.semantic_rank, s.fts_rank
  FROM postvec.search(
         'public.legacy', 'body',
         'embedding migration risk',
         limit_n => 20
       ) AS s
  JOIN public.legacy AS d
    ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

:::: tip Expected
The adopted column is not rewritten and `owns_vector_column` remains
false. Semantic ranks are present, and the query vector has the stored
target dimension (1536 for classic ada-002).
::::

To return an error for a missing route during validation:

```sql
SET postvec.search_degrade_to_fts = off;

SELECT *
  FROM postvec.search(
         'public.legacy', 'body',
         'known semantic query'
       );
```

With degradation on (the default), a route or inference failure emits a
warning and returns lexical results. `semantic_rank IS NULL` on every
result is the sign that the semantic leg did not run.

## Model requirements

An executable route needs all three parts in one local engine:

- one direct embed model for the bridge/source space
- one converter from that source to the stored target space
- one `embed-bridge` executor

On an **embedded** host, inspect the catalogue and pull the converter's
inventory name. Dependencies bring the companion embed model and
executor:

```bash
postvec model ls --available
converter_name='replace-with-catalogue-name'
sudo postvec model pull "$converter_name" --dry-run
sudo postvec model pull "$converter_name" --yes          # installed, deactivated
sudo postvec model activate "$converter_name" --yes      # enables the whole chain
sudo postvec doctor --database app --deep
```

`pull` installs the closure deactivated. `activate` on the converter
enables its deactivated dependencies with it (the embed model and the
`embed-bridge` executor). The engine refuses to load a chain that
contains a deactivated member, and a restart would not make one
resident either.

Not every source/target pair is present in the public subset. The
private catalogue holds the broader conversion inventory.

On **remote**, models are administered on the `postvec-server` nodes;
local `postvec model pull` is refused. At least one node must host the
complete embed model, converter and bridge chain. Pieces discovered on
different nodes do not form an executable route. Missing-chain errors
are failover-eligible, so postvec can try another configured endpoint.

After any inventory change:

```sql
SELECT postvec.refresh_models();

SELECT name, model_type, source_model, target_model, target_dim
  FROM postvec.models
 ORDER BY model_type, name;
```

`refresh_models()` is administrative and is not executable by PUBLIC
without an explicit grant.

## Latency and timeouts

Bridge search is one database-to-engine RPC, but two inference stages
run inside the engine. The embedder, converter and bridge executor
should remain warm. `postvec.query_timeout_ms` should reflect measured
bridge latency.

FTS degradation can be disabled during timeout diagnosis. Warnings and
returned rank columns also indicate whether the semantic leg ran.

## Migration after bridge adoption

| Choice | Stored corpus | New queries | New writes with `sync => true` |
|---|---|---|---|
| Bridge search | unchanged | embedded, then converted into old space | embedded, then converted into old space |
| [`migrate()`](/docs/guides/migrate) | converted to the new space | embedded directly in new space | embedded directly in new space |

A staged migration can adopt the existing column, validate search
through the bridge and later migrate the stored vectors in place.

:::: danger Confirm the source model separately from the dimension
An incorrect 1536-dimensional model still represents an incompatible
vector space. Confirm the model that produced the column before
synchronized writes or conversion.
::::

:::: danger Existing rows stay in the old space
Bridge search produces new vectors (queries and synchronized future
writes) in the old target space. Existing rows remain exactly as they
were.
::::
