---
title: Search without migrating
description: Adopt vectors in a retired or provider-only model space and search them through automatic embed-bridge routing.
---

# Search without migrating

An existing corpus does not have to move before postvec can search it. If the
column is already in a retired or provider-only space, postvec can create each
new query vector locally and convert that vector into the stored space.

For example, an `openai-text-embedding-ada-002` corpus can remain byte-for-byte
unchanged. `search()` can embed the query with an available open model, convert
that one vector to ada-002 space, then run pgvector and full-text search against
the existing table. No OpenAI call and no corpus re-embed.

Bridge search is normal `search()` behavior. There is no `search_bridge()` and
no bridge parameter in application SQL.

## The route

```text
query text
  → direct local embed model
      → converter into the stored model space
          → existing vector column
```

Resolution is deterministic:

1. A direct embed model for the declared target wins.
2. Otherwise postvec chooses the lexicographically first converter into that
   target whose source is directly embeddable, then uses an `embed-bridge`
   executor.
3. If neither route exists, `search()` follows the configured FTS-degradation
   policy; administrative and write paths fail or retry normally.

If a direct embed route becomes available later, it takes precedence after the
next model refresh. The resolved route is inventory state, so postvec does not
persist it in the table registry.

## 1. Verify the stored space

The vector dimension is a fact. The model name is your provenance assertion.
Check both before adoption:

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

::: warning Provenance cannot be inferred from the bytes
Matching `vector(1536)` only rules out models with another dimension. It does
not prove that ada-002 produced the vectors. A wrong model assertion yields
plausible but invalid ranks.
:::

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

`backfill => 'none'` protects the existing population. `sync => true` keeps
future `INSERT` and `UPDATE` operations in the same target space through the
same bridge route.

For a frozen or `NOT NULL` legacy vector column, observe it without a write
path:

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

## 3. Search normally

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

::: tip Expected
The adopted column is not rewritten and `owns_vector_column` remains false.
Semantic ranks are present, and the query vector has the stored target
dimension (1536 for classic ada-002).
:::

To make a missing route loud while validating the setup:

```sql
SET postvec.search_degrade_to_fts = off;

SELECT *
  FROM postvec.search(
         'public.legacy', 'body',
         'known semantic query'
       );
```

With degradation on (the default), a route or inference failure emits a warning
and returns lexical results. `semantic_rank IS NULL` on every result is the
observable sign that the semantic leg did not run.

## Model requirements

An executable route needs all three parts:

- one direct embed model for the bridge/source space
- one converter from that source to the stored target space
- one `embed-bridge` executor

On an **embedded** host, inspect the catalogue and pull the converter's
inventory name. Dependencies bring the companion embed model and executor:

```bash
sudo postvec model ls --available
converter_name='replace-with-catalogue-name'
sudo postvec model pull "$converter_name" --dry-run
sudo postvec model pull "$converter_name" --yes
sudo postvec doctor --database app --deep
```

Not every source/target pair is present in the public subset. The organisation
catalogue contains the broader conversion inventory.

On **remote**, the ninference fleet is administered separately; local
`postvec model pull` is refused. At least one node must host the complete embed
model, converter, and bridge chain. Pieces discovered on different nodes do not
form an executable route. Missing-chain errors are failover-eligible, so
postvec can try another configured endpoint.

After any inventory change:

```sql
SELECT postvec.refresh_models();

SELECT name, model_type, source_model, target_model, target_dim
  FROM postvec.models
 ORDER BY model_type, name;
```

`refresh_models()` is administrative and not executable by PUBLIC unless you
grant it.

## Latency and timeouts

Bridge search is one database-to-engine RPC, but two inference stages run
inside the engine. Keep the embedder, converter, and bridge executor warm. Set
`postvec.query_timeout_ms` from measured bridge latency rather than from direct
embedding latency alone.

If a timeout quietly reduces semantic recall, turn FTS degradation off during
diagnosis or monitor warnings plus the returned rank columns.

## Bridge now, migrate later

| Choice | Stored corpus | New queries | New writes with `sync => true` |
|---|---|---|---|
| Bridge search | unchanged | embedded, then converted into old space | embedded, then converted into old space |
| [`migrate()`](/docs/guides/migrate) | converted to the new space | embedded directly in new space | embedded directly in new space |

The lowest-change adoption sequence is: adopt the existing column, validate
search through the bridge, then migrate in place later if and when the database
is ready.

## Don't

::: danger Don't treat a dimension match as provenance
The wrong 1536-dimensional model is still the wrong vector space. Confirm the
model that produced the column before synchronized writes or conversion.
:::

::: danger Don't expect bridge search to upgrade stored rows
It only produces new vectors—queries and synchronized future writes—in the old
target space. Existing rows remain exactly as they were.
:::
