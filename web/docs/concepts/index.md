---
title: How it works
description: Runtime shape, write path and common operational constraints.
---

# How it works

Most operational surprises come from the same few facts: empty vectors
right after `INSERT`, a worker that never started and `search()` that
takes too long.

## Runtime

```
PostgreSQL cluster
┌─────────────────────────────────────────────────────────────┐
│ launcher  (one per cluster, requires shared_preload)        │
│   ├─ one worker per name in postvec.database                │
│   └─ embedded mode: one shared InferenceEngine              │
│                                                             │
│ source table  docs(id, body, body_semantic vector(N))       │
│   AFTER INSERT/UPDATE/TRUNCATE ──▶ postvec.jobs             │
│                                                             │
│ worker  claim/read  →  inference  →  write-back             │
└────────────────────────┬───────────────────┬────────────────┘
                         │ gRPC              │ HTTP /config
                         ▼                   ▼
            remote ninference, or the launcher on loopback
```

`CREATE EXTENSION` exposes the SQL surface immediately. Automatic sync
still needs the preloaded launcher and a worker for that database. If
`shared_preload_libraries` does not include `postvec`, there is no
worker.

## Write path

```
INSERT / UPDATE
  → trigger inserts (registry_id, pk_value) into postvec.jobs
      → pending work for the same row coalesces
          → worker reads current text, commits, calls inference
              → worker writes the vector (or retries / dead-letters)
```

Between the application commit and worker write-back, the vector is NULL
or stale. A committed write arms an at-commit latch and nudges the
worker, so the gap is normally just inference time.
`postvec.poll_interval_ms` (default 5000) is only the backstop. See
[eventual consistency](/docs/concepts/consistency).

`search()` is the exception: it embeds the **query** synchronously.
Query embedding is the only synchronous inference step on the search
path.

## Durable and transient state

| Durable (dumped) | Cache (not dumped) |
|---|---|
| registry, jobs, jobs_dead, migrations | `postvec.models`, worker heartbeat |

A restore needs matching files, cluster configuration, a model refresh
and `doctor`. [Backup](/docs/guides/backup) has the checklist.

## Operational constraints

1. **Vectors fill after commit.** `status()` reports readiness.
2. **The worker needs preload and a restart.** Every database named in
   `postvec.database` must exist. A missing name makes the worker fail
   and respawn about every 15 seconds.
3. **ANN indexes are opt-in.** Missing ANN is a common reason search is
   slow. `index_mode => 'auto'` is available; the build is blocking.
4. **A new library and `ALTER EXTENSION` belong in one window.** Until
   then the worker pauses.
5. **`adopt()`'s `model` is an assertion.** The wrong name makes
   `search()` embed into an incompatible space.

## Related documentation

- [Embedded vs remote](/docs/concepts/modes)
- [Embedding debt and vector lock-in](/docs/concepts/lock-in)
- [Eventual consistency](/docs/concepts/consistency)
- [Enable a column](/docs/guides/enable)
