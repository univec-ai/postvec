---
title: Search a retired space (SQL)
description: Adopt vectors in a retired or provider-only model space and search them through local embed-bridge routing.
---

# Search a retired space

When a column already holds vectors from a retired or provider-only
model, keep those vectors and search them as they are. This is the usual
first path: adopt, then search.

`adopt()` registers the column. `search()` then embeds each query with
an available local model and converts that one vector into the stored
space (embed-bridge). An `openai-text-embedding-ada-002` corpus stays
byte-for-byte unchanged.

If later you want the stored space itself to change,
[`migrate()`](/docs/guides/migrate) converts the corpus in place.

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
3. If neither route exists, `search()` falls back to lexical results
   (the default) or returns an error if `search_degrade_to_fts` is off.

If a direct embed route becomes available later, it takes precedence
after the next model refresh.

:::: warning A provider key ends the bridge
Configuring an [external provider](/docs/models/providers) for the stored
space makes a direct route exist. The column then stops bridging and starts
sending its source text to that provider, with no SQL change. `postvec
provider add` lists the affected columns and requires an acknowledgement
before it writes the file.
::::

:::: info Hosted converters are direct routes
A [UniVec hosted converter](/docs/models/univec) serves `migrate()` or
`convert()`. Embed-bridge resolution runs inside the local engine and
uses local models.
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

:::: warning Provenance is an operator assertion
Matching `vector(1536)` only rules out models with another dimension.
An incorrect model assertion yields plausible but invalid ranks.
::::

## 2. Adopt, keep stored bytes

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

For a frozen or `NOT NULL` legacy vector column, observe it (no write
path):

```sql
SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'openai-text-embedding-ada-002',
  sync => false,
  backfill => 'none'
);
```

Observed mode can search. Future writes stay the application's
responsibility. See [`adopt()`](/docs/guides/adopt) before migrating an
observed entry.

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
Stored bytes stay as they are and `owns_vector_column` remains false.
Semantic ranks are present, and the query vector has the stored
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

On an embedded host, pull the converter (dependencies bring the
companion embed model and executor). On remote, administer the same
chain on [postvec-server](/docs/server/models). Commands:
[search a retired space (CLI)](/docs/guides/bridge-cli).

After any inventory change:

```sql
SELECT postvec.refresh_models();

SELECT name, model_type, source_model, target_model, target_dim
  FROM postvec.models
 ORDER BY model_type, name;
```

`refresh_models()` is administrative. Grant it explicitly if an
application role must call it.

## Latency and timeouts

Bridge search is one database-to-engine RPC, but two inference stages
run inside the engine. The embedder, converter and bridge executor
should remain warm. `postvec.query_timeout_ms` should reflect measured
bridge latency.

FTS degradation can be disabled during timeout diagnosis. Warnings and
returned rank columns also indicate whether the semantic leg ran.

## Later: change the stored model

Choose between keeping the stored space and moving it:

| Choice | Stored corpus | New queries | New writes with `sync => true` |
|---|---|---|---|
| Search a retired space (embed-bridge) | unchanged | embedded, then converted into old space | embedded, then converted into old space |
| [`migrate()`](/docs/guides/migrate) | converted to the new space | embedded directly in new space | embedded directly in new space |

A staged path: adopt the existing column, validate search through the
bridge, then migrate the stored vectors in place.

:::: danger Confirm the source model separately from the dimension
An incorrect 1536-dimensional model still represents an incompatible
vector space. Confirm the model that produced the column before
synchronized writes or conversion.
::::

:::: danger Existing rows stay in the old space
Bridge search embeds queries and synchronized writes into the old target
space. Existing rows remain exactly as they were.
::::

- [Search a retired space (CLI)](/docs/guides/bridge-cli)
- [Adopt](/docs/guides/adopt)
- [Change the stored model](/docs/guides/migrate)
