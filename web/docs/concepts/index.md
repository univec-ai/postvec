---
title: How it works
description: Runtime shape, write path, and common operational constraints.
---

# How it works

A small part of the runtime model explains the most common operational issues:
empty vectors immediately after `INSERT`, an inactive worker, and slow
`search()` calls.

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

`CREATE EXTENSION` exposes SQL immediately. Automatic sync requires the
preloaded launcher **and** a worker for that database. If
`shared_preload_libraries` does not include `postvec`, there is no worker.

## Write path

```
INSERT / UPDATE
  → trigger inserts (registry_id, pk_value) into postvec.jobs
      → pending work for the same row coalesces
          → worker reads current text, commits, calls inference
              → worker writes the vector (or retries / dead-letters)
```

Between the application commit and worker write-back, the vector is NULL or
stale. The interval is normally `postvec.poll_interval_ms` plus inference time.
See [eventual consistency](/docs/concepts/consistency).

`search()` is the exception: it embeds the **query** synchronously. Query
embedding is the only synchronous inference operation in the search path.

## Durable and transient state

| Durable (dumped) | Cache (not dumped) |
|---|---|
| registry, jobs, jobs_dead, migrations | `postvec.models`, worker heartbeat |

Restoration requires matching files, cluster configuration, a model refresh,
and `doctor`. [Backup](/docs/guides/backup) contains the checklist.

## Operational constraints

1. **Vectors fill later.** `INSERT` completion does not imply that the vector
   column is filled. Readiness is available through `status()`.
2. **No worker without preload + restart.** Every database named in
   `postvec.database` must exist. A missing name causes the worker to fail and
   respawn approximately every 15 seconds.
3. **No ANN index by default.** A missing ANN index is a common cause of slow
   search. `index_mode => 'auto'` is opt-in because the build is blocking.
4. **Replacing `postvec.so` does not upgrade SQL.** Restart and
   `ALTER EXTENSION postvec UPDATE` belong in one window. Until then the
   worker pauses.
5. **`adopt()`'s `model` is an assertion**, not a proof. An incorrect name
   makes `search()` embed into an incompatible space.

## Related documentation

- [Embedded vs remote](/docs/concepts/modes)
- [Embedding debt and vector lock-in](/docs/concepts/lock-in)
- [Eventual consistency](/docs/concepts/consistency)
- [Enable a column](/docs/guides/enable)
