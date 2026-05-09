---
title: Indexes
description: Manual, automatic and immediate ANN index modes.
---

# Indexes

Index creation is manual by default. A missing ANN index is the usual
reason `search()` is slow.

```sql
SELECT postvec.create_vector_index('public.docs', 'body');
```

:::: tip Expected
`status().has_vector_index` becomes true. If a usable index already
exists (including one you built), the function does nothing and does
not claim it.
::::

Only indexes **created by postvec** carry the extension-dependency stamp
and are dropped at teardown. A name you chose yourself does not transfer
ownership.

## Modes (`index_mode` on `enable` / `adopt`)

| Mode | When the index appears | Lock |
|---|---|---|
| `manual` (default) | On an explicit `create_vector_index()` or `CREATE INDEX CONCURRENTLY` | Selected by you |
| `immediate` | In the `enable()` / `adopt()` transaction | Blocking `CREATE INDEX` |
| `auto` | After the worker sees the queue drain | Blocking, **and** occupies the only worker |

Both non-manual modes are blocking. PostgreSQL forbids `CONCURRENTLY`
inside those transactions. `auto` also pauses embedding, migrations,
cursor backfill and heartbeats for that database while it builds.

For that reason `auto` is opt-in and is meant for small, quiet tables.
Large or write-heavy tables should stay on `manual` and use:

```sql
CREATE INDEX CONCURRENTLY docs_body_hnsw
  ON public.docs
  USING hnsw (body_semantic vector_cosine_ops);
```

Match the opclass to the entry's `distance` (`vector_cosine_ops`,
`vector_l2_ops`, `vector_ip_ops`). Above 2000 dimensions both
non-manual modes refuse and suggest a `halfvec` expression index.

`immediate` is refused for [chunked](/docs/guides/chunking) entries.
Index the destination after backfill.

## Auto-build eligibility

Active · `index_error IS NULL` · no live migration (including
`awaiting_index`) · not cursor-backfilling · no pending or claimed job ·
dim <= 2000 · no usable index yet.

A failed automatic build records `status().index_error` and stops
retrying. An explicit `create_vector_index()` call clears the error
after readiness is satisfied.

`doctor` ranks index findings: failed automatic build -> wrong opclass
-> manual with no index -> auto still waiting (informational). A
suitable user-created index passes.

## Constraints

:::: danger `auto` performs a blocking index build
The worker will `CREATE INDEX` (not concurrently) and stop embedding for
the duration.
::::

:::: danger Index ownership is based on creation, not naming
User-created indexes remain user-owned. Dropping a postvec-created index
from an `auto` entry causes the worker to rebuild it.
::::
