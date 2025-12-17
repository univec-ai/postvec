---
title: Usage overview
description: SQL operations from column registration through search and migration.
---

# Usage overview

Registration, search, filters and migration are SQL. The CLI configures the cluster and, in embedded mode, the model inventory. There is no CLI entry point for `enable()`, `search()` or `set_format()`.

| Task | SQL | CLI counterpart |
|---|---|---|
| Attach a new column | [`enable()`](/docs/guides/enable) | `doctor` / `status()` |
| Search | [`search()`](/docs/guides/search) | - |
| Restrict by metadata | [`filter`](/docs/guides/filters) | - |
| Embed title + body | [`set_format()`](/docs/guides/templates) | - |
| Long documents | [`chunking`](/docs/guides/chunking) | - |
| Register existing vectors | [`adopt()`](/docs/guides/adopt) | - |
| Query a locked space without migrating | [bridge](/docs/guides/bridge) | pull the converter; dependencies bring the embed model + `embed-bridge` |
| Change model | [`migrate()`](/docs/guides/migrate) | - |
| Speed up search | [`create_vector_index()`](/docs/guides/indexes) | `doctor` index checks |
| Re-drive failures | [`retry_dead()`](/docs/guides/retry) | `doctor` `queue.dead` |
| See health | [`status()` / `stats()`](/docs/guides/status) | `postvec doctor` |
| Dump / restore | [Backup](/docs/guides/backup) | `setup` after restore |

Functions live in the `postvec` schema. The extension does not add itself to `search_path`, so every call stays schema-qualified: `postvec.*`.

## Minimal workflow

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
The second row ranks first. `has_vector_index` is true. No dead jobs.
::::

## Roles

Table ownership or superuser is required for the management verbs: `enable`, `adopt`, `disable`, `migrate`, `set_format`, `retry_dead` and `create_vector_index`. `uninstall()` is superuser only. `search()` is executable by PUBLIC. `embed` / `convert` / `refresh_models` are revoked from PUBLIC.

## Guides in this section

1. [Enable](/docs/guides/enable) a new column, or [adopt](/docs/guides/adopt) one that already exists.
2. [Search](/docs/guides/search), then add [filters](/docs/guides/filters) and [templates](/docs/guides/templates) as needed.
3. [Chunk](/docs/guides/chunking) long documents.
4. [Bridge](/docs/guides/bridge) an existing space, or [migrate](/docs/guides/migrate) it.
5. [Index](/docs/guides/indexes), [retry](/docs/guides/retry) dead work and [watch](/docs/guides/status) the worker.
