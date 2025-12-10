---
title: Usage overview
description: SQL operations from column registration through search and migration.
---

# Usage overview

Application work is SQL. The CLI configures the cluster and, in embedded
mode, the models. There is no CLI flag that calls `enable()`, `search()`,
or `set_format()`.

| Task | SQL | CLI counterpart |
|---|---|---|
| Attach a new column | [`enable()`](/docs/guides/enable) | `doctor` / `status()` |
| Search | [`search()`](/docs/guides/search) | — |
| Restrict by metadata | [`filter`](/docs/guides/filters) | — |
| Embed title + body | [`set_format()`](/docs/guides/templates) | — |
| Long documents | [`chunking`](/docs/guides/chunking) | — |
| Register existing vectors | [`adopt()`](/docs/guides/adopt) | — |
| Query a locked space without migrating | [bridge](/docs/guides/bridge) | pull the converter; dependencies bring the embed model + `embed-bridge` |
| Change model | [`migrate()`](/docs/guides/migrate) | — |
| Speed up search | [`create_vector_index()`](/docs/guides/indexes) | `doctor` index checks |
| Re-drive failures | [`retry_dead()`](/docs/guides/retry) | `doctor` `queue.dead` |
| See health | [`status()` / `stats()`](/docs/guides/status) | `postvec doctor` |
| Dump / restore | [Backup](/docs/guides/backup) | `setup` after restore |

Every function is schema-qualified: `postvec.*`. Nothing is added to
`search_path`.

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

::: tip Expected
The second row ranks first. `has_vector_index` is true. No dead jobs.
:::

## Roles

Management verbs (`enable`, `adopt`, `disable`, `migrate`, `set_format`,
`retry_dead`, `create_vector_index`) require table ownership or superuser.
`uninstall()` is superuser only. `search()` is executable by PUBLIC.
`embed` / `convert` / `refresh_models` are revoked from PUBLIC.

## Related documentation

- [Enable a column](/docs/guides/enable) — create and maintain a vector column
- [Adopt existing vectors](/docs/guides/adopt) — register an existing vector
  column
