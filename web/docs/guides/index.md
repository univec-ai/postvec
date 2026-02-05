---
title: Usage overview
description: SQL operations from column registration through search and migration.
---

# Usage overview

Registration, search, filters and migration are SQL. The CLI configures
the cluster and, in embedded mode, the model inventory.

If the table already exists, start at [Which SQL call](/docs/guides/starting).
If you are filling a new table, the block below is the whole first pass.

## First pass

```sql
CREATE TABLE docs (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    body text
);

SELECT postvec.enable(
    'public.docs', 'body',
    model => 'sentence-transformers-all-minilm-l6-v2',
    create_fts_index => true
);

INSERT INTO docs (body) VALUES
  ('quarterly revenue guidance was raised after strong subscription growth'),
  ('migrating embedding models normally requires re-embedding source text'),
  ('the office plants need watering twice a week');

-- wait until pending_jobs = 0
SELECT relation, pending_jobs, dead_jobs FROM postvec.status();

SELECT postvec.create_vector_index('public.docs', 'body');

SELECT d.id, d.body, s.rrf_score
  FROM postvec.search(
         'public.docs', 'body',
         'switching AI models without redoing the work'
       ) AS s
  JOIN public.docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

:::: tip Expected
The engineering row ranks first. `has_vector_index` is true. No dead
jobs.
::::

Vectors fill **after** the inserting transaction commits. Poll
`status()` in a later transaction. See
[eventual consistency](/docs/concepts/consistency).

## Guides in this section

Use, in roughly this order:

1. [Enable](/docs/guides/enable) a new column, or [adopt](/docs/guides/adopt)
   one that already exists.
2. [Search](/docs/guides/search), then [filters](/docs/guides/filters) and
   an [index](/docs/guides/indexes).
3. [Templates](/docs/guides/templates) if short texts need row context.
   [Chunk](/docs/guides/chunking) if the source is long.
4. [Bridge](/docs/guides/bridge) an existing space, or
   [migrate](/docs/guides/migrate) it.

Operate, when something needs watching:

- [Status](/docs/guides/status) and `postvec doctor`
- [`retry_dead()`](/docs/guides/retry)
- [Backup](/docs/guides/backup)
- [One-shot helpers](/docs/guides/helpers) (`embed` / `convert`)

## Roles

Table ownership or superuser is required for the management verbs:
`enable`, `adopt`, `disable`, `migrate`, `set_format`, `retry_dead` and
`create_vector_index`. `uninstall()` is superuser only. `search()` is
executable by PUBLIC. `embed` / `convert` / `refresh_models` are revoked
from PUBLIC.
