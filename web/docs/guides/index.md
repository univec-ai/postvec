---
title: Usage overview
description: SQL operations from column registration through search and migration.
---

# Usage overview

Registration, search, filters and migration are SQL. The CLI configures
the cluster and, in embedded mode, the model inventory. Configure a
[hosted embedding API](/docs/models/providers) with
`postvec provider add` first; the SQL after that is the same.

Existing tables: [SQL functions](/docs/guides/starting). New table: the
sequence below.

- `enable()` - adds a shadow vector column and starts syncing
- `adopt()` - registers an existing vector column; stored bytes stay
- `search()` - hybrid full-text + vector ranking, one query
- `create_vector_index()` - builds the ANN index `search()` uses
- `set_format()` - changes the embedding template and refreshes every row
- `migrate()` - converts stored vectors to another model, then waits for finalize

## Example

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

- [Enable](/docs/guides/enable) / [adopt](/docs/guides/adopt)
- [External providers](/docs/models/providers), then the same `enable()`
- [Search](/docs/guides/search), [filters](/docs/guides/filters),
  [indexes](/docs/guides/indexes)
- [Templates](/docs/guides/templates), [chunking](/docs/guides/chunking)
- [Bridge](/docs/guides/bridge) / [migrate](/docs/guides/migrate)
- [Status](/docs/guides/status), [`retry_dead()`](/docs/guides/retry),
  [backup](/docs/guides/backup), [helpers](/docs/guides/helpers)

## Roles

Table ownership or superuser is required for the management verbs:
`enable`, `adopt`, `disable`, `migrate`, `set_format`, `retry_dead` and
`create_vector_index`. `uninstall()` is superuser only. `search()` is
executable by PUBLIC. `embed` / `convert` / `refresh_models` are revoked
from PUBLIC.
