---
title: Query an existing space
description: Adopt a locked corpus (ada-002 and friends) and search it without migrating — embed-bridge produces query vectors in that space.
---

# Query an existing space

You do not have to migrate to use postvec. If the corpus is already
vectorized — `ada-002`, a retired provider, any space the catalogue can
*target* — you can leave those bytes alone and still run hybrid search.

postvec does this with the same `model =>` name you would use on
`enable()`. Resolution is two-tier:

1. a direct embed model with that name, or
2. a converter *targeting* that name plus an `embed-bridge` executor

`search()` embeds the **query** through that route, so the query lands in
the same space as the stored vectors. No re-embed of the corpus. No
call to the original provider.

This is the "don't pay the debt today" path. Convert later with
[`migrate()`](/docs/guides/migrate) if you want.

## Adopt, then search

```sql
-- Name must appear in postvec.models (direct embed or a bridge target).
SELECT name, model_type, target_model, target_dim
  FROM postvec.models
 ORDER BY name;

SELECT postvec.adopt(
  'public.legacy', 'body',
  vector_column => 'embedding',
  model => 'text-embedding-ada-002'   -- example; use the catalogue name
);

SELECT d.id, d.body, s.rrf_score
  FROM postvec.search(
         'public.legacy', 'body',
         'quarterly guidance'
       ) AS s
  JOIN public.legacy AS d ON d.id = s.pk_value::bigint;
```

::: tip Expected
`owns_vector_column` is false. Existing vectors are not rewritten.
`search()` returns ranked rows. The query vector has the column's
dimension (1536 for classic ada-002).
:::

New writes, if `sync => true`, also go through the same route — still
no provider API in PostgreSQL.

Read-only is fine too: `adopt(..., sync => false, backfill => 'none')`.
See [adopt](/docs/guides/adopt).

## What has to be loaded

On **embedded**, pull the converter, its source embed model, and
`embed-bridge`. Dependencies are automatic:

```bash
sudo postvec model ls --available
sudo postvec model pull text-embedding-ada-002 --dry-run
sudo postvec model pull text-embedding-ada-002 --yes
```

The plan labels companions. A public-registry subset may not include
every commercial target; organisation accounts see the full catalogue.

On **remote**, the ninference node must already advertise that route.
`postvec model pull` against a remote cluster is refused.

## When the route is missing

`enable()` / `adopt()` / `search()` refuse an unknown or non-embeddable
name. `TARGET_RESTRICTED` means the engine is deliberately not allowed
to bridge *into* that target (licence policy on current commercial
spaces). Deprecated spaces such as ada-002 are the intended rescue
targets.

```sql
SELECT postvec.refresh_models();
SELECT name, model_type, target_model FROM postvec.models;
```

## Don't

::: danger Don't adopt with the wrong model name
The column's dimension is fact; the name is an assertion. A wrong name
makes `search()` embed the query into the wrong space — confident, bad
ranks. Check `target_dim` against `vector_dims(embedding)`.
:::

::: danger Don't expect every ada-002 row to be "upgraded"
Nothing in this path rewrites stored vectors. They stay whatever they
were. Bridge only produces **new** vectors (queries, and new writes) in
that space.
:::
