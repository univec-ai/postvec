---
title: Indexes
description: Manual, auto, and immediate ANN index modes — and why manual is the default.
---

# Indexes

No vector index is built by default. "search() is slow" is almost always
this.

```sql
SELECT postvec.create_vector_index('public.docs', 'body');
```

`create_vector_index()` is **readiness-first**. If a usable index with the
right opclass already exists — including one you built yourself — it
does nothing and does not claim it. Only indexes postvec creates carry
the extension-dependency stamp and are dropped at teardown.

## Modes (`index_mode` on `enable` / `adopt`)

| Mode | When the index appears | Lock |
|---|---|---|
| `manual` (default) | When you call `create_vector_index()` or `CREATE INDEX CONCURRENTLY` | Your choice |
| `immediate` | In the `enable()`/`adopt()` transaction | Blocking `CREATE INDEX` |
| `auto` | After the worker sees the queue drain | Blocking, **and** occupies the only worker |

Both non-manual modes are blocking. PostgreSQL forbids `CONCURRENTLY`
inside those transactions. `auto` additionally pauses embedding,
migrations, cursor backfill, and heartbeats for that database while it
builds.

That is why `auto` is opt-in. Use it on a small, quiet table. On a large
or write-heavy table keep `manual` and run:

```sql
CREATE INDEX CONCURRENTLY docs_body_hnsw
  ON public.docs
  USING hnsw (body_semantic vector_cosine_ops);
```

Match the opclass to the entry's `distance` (`vector_cosine_ops`,
`vector_l2_ops`, `vector_ip_ops`). Above 2000 dimensions both non-manual
modes refuse and suggest a `halfvec` expression index.

`immediate` is refused for [chunked](/docs/guides/chunking) entries —
index the destination after backfill.

## Auto-build eligibility

Active · `index_error IS NULL` · no live migration (including
`awaiting_index`) · not cursor-backfilling · no pending or claimed job ·
dim ≤ 2000 · no usable index yet.

A failed auto-build parks in `status().index_error` and stops retrying
until you call `create_vector_index()` (which clears the error if
readiness is now satisfied).

`doctor` ranks index findings: parked auto-build → wrong opclass →
manual with no index → auto still waiting (informational). An index you
built yourself passes.

## Don't

::: danger Don't set `index_mode => 'auto'` on a 20 M row table
The worker will `CREATE INDEX` (not concurrently) and stop embedding for
the duration.
:::

::: danger Don't drop "the postvec index" by name
Ownership is by creation, not by name. Dropping a user-built index is
your affair. Dropping a postvec-built one on an `auto` entry makes the
worker try to put it back.
:::
