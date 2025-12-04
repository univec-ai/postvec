---
title: How it works
description: Runtime shape, write path, and the two constraints that explain most surprises.
---

# How it works

You do not need the internals to use postvec. You do need the shape, because
it explains the three surprises people hit first: empty vectors just after
`INSERT`, a silent worker, and a slow `search()`.

## Runtime

```
PostgreSQL cluster
┌─────────────────────────────────────────────────────────────┐
│ launcher  (one per cluster, requires shared_preload)        │
│   ├─ one worker per name in postvec.database                │
│   └─ embedded mode: one shared InferenceEngine              │
│                                                             │
│ your table  docs(id, body, body_semantic vector(N))         │
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

Between your commit and the write-back the vector is NULL or stale. That
window is normally `postvec.poll_interval_ms` plus inference time. See
[eventual consistency](/docs/concepts/consistency).

`search()` is the exception: it embeds the **query** synchronously. That is
the one inline network call on the query path.

## What lives where

| Durable (dumped) | Cache (not dumped) |
|---|---|
| registry, jobs, jobs_dead, migrations | `postvec.models`, worker heartbeat |

After restore you reinstall matching files, restore cluster configuration,
refresh models, and run `doctor`. [Backup](/docs/guides/backup) has the
checklist.

## The five things that trip people up

1. **Vectors fill later.** Poll `status()`; do not assume the `INSERT`
   returned a filled column.
2. **No worker without preload + restart.** Also: name only databases that
   exist. A missing name in `postvec.database` FATAL-loops every ~15 s.
3. **No ANN index by default.** Slow search is almost always this.
   `index_mode => 'auto'` is opt-in because the build is blocking.
4. **Replacing `postvec.so` does not upgrade SQL.** Restart and
   `ALTER EXTENSION postvec UPDATE` belong in one window. Until then the
   worker parks.
5. **`adopt()`'s `model` is an assertion**, not a proof. A wrong name makes
   `search()` embed into the wrong space.

## Next

- [Embedded vs remote](/docs/concepts/modes)
- [Embedding debt and vector lock-in](/docs/concepts/lock-in)
- [Eventual consistency](/docs/concepts/consistency)
- [Enable a column](/docs/guides/enable)
