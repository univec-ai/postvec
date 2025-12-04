---
title: Filters
description: AND-only JSON filters pushed into both search legs before ranking.
---

# Filters

`filter` is a JSON object. Every key is a column on the **source** table.
Conditions are AND-ed and applied **inside both candidate legs** before
ranking and `LIMIT`. A bad filter is rejected *before* the query is
embedded, so it costs no inference.

```sql
SELECT d.body, s.rrf_score
  FROM postvec.search(
         'public.docs', 'body', 'model migration',
         filter => '{
           "category": {"in": ["engineering", "finance"]},
           "id": {"gte": 1}
         }'::jsonb
       ) AS s
  JOIN public.docs AS d ON d.id = s.pk_value::bigint;
```

::: tip Expected
Only matching rows are candidates. Rank numbers are computed on that
subset, not computed globally and then discarded.
:::

`NULL` and `{}` both mean "no filter".

## Grammar

| JSON | SQL |
|---|---|
| `"category": "finance"` | `category = 'finance'` |
| `"archived_at": null` | `IS NULL` |
| `"archived_at": {"is_not": null}` | `IS NOT NULL` |
| `"price": {"neq": 0, "lte": 100}` | operators AND-compose on one column |
| `"region": ["EU","UK"]` | shorthand for `{"in": ["EU","UK"]}` |
| `"title": {"like": "Q3%"}` | `LIKE` (`ilike` too) |

Closed operator set: `neq`, `gt`, `gte`, `lt`, `lte`, `in`, `like`,
`ilike`, `is_not` (null only). Equality is the scalar shorthand — **there
is no `eq`**.

Caps: 64 KiB serialized, 32 columns, 256 `in` values.

## Highly selective filters

HNSW can under-recall when most of the index is excluded. Raise
`candidates`, `hnsw.ef_search`, or `hnsw.max_scan_tuples`. There is no
selectivity estimator on purpose.

## Don't

::: danger Don't pass a SQL fragment
`filter => 'category = ''finance'''` is refused. The injection surface is
why this is JSON, not a predicate string.
:::

::: danger Don't write `"category": {"eq": "finance"}`
There is no `eq`. Use `"category": "finance"`.
:::

::: danger Don't put `null` inside `in` or a comparison
Use the scalar `null` / `{"is_not": null}` forms.
:::

Unknown columns, unknown operators, empty operator objects, nested
objects as values, and values the column's type rejects
(`pg_input_is_valid`) are all refused with a specific error.
