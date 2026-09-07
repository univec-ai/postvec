---
title: Status (SQL)
description: Health signals from status() and stats().
---

# Status and health

`status()` is per enabled column. `stats()` is the worker process.
Host-side checks: [status (CLI)](/docs/guides/status-cli) (`postvec
doctor`).

Start here when vectors stay NULL, search is lexical-only or a
migration looks stuck.

## `status()` - per entry

```sql
SELECT relation, model, dim, state,
       pending_jobs, dead_jobs,
       has_vector_index, index_error,
       worker_last_beat, last_error,
       chunking, pending_refresh_jobs, pending_embed_jobs,
       lexical_docs, lexical_stats_age_seconds, lexical_error
  FROM postvec.status();
```

| Check | Field |
|---|---|
| Backfill done | `pending_jobs = 0` (and refresh/embed = 0 if chunked) |
| Failures | `dead_jobs`, `last_error` |
| Worker alive | `worker_last_beat` **advances** |
| Search will be fast | `has_vector_index` |
| Automatic index build paused after failure | `index_error` |
| BM25 corpus stats | `lexical_docs`, `lexical_stats_age_seconds`, `lexical_error` |
| Quarantined entry | `state` (source/vector/template column vanished) |

A heartbeat row survives a worker crash. Health requires the timestamp to advance between samples.

## `stats()` - the worker

```sql
SELECT worker_pid, worker_started_at, worker_last_beat,
       jobs_embedded, jobs_retried, jobs_dead_lettered,
       queue_pending, queue_claimed, queue_dead,
       migrations_running, documents_chunked, chunks_created,
       worker_last_error
  FROM postvec.stats();
```

Counters are process-lifetime. They reset when the worker respawns.

## Version

```sql
SELECT postvec.version();
SELECT postvec.build_info();  -- {version, diagnostics_api, features.embedded}
```

If the worker log asks for `ALTER EXTENSION`, the library and installed
SQL disagree. See [upgrade](/docs/install/upgrade).
[doctor](/docs/guides/status-cli) reports the same.

- [Status (CLI)](/docs/guides/status-cli)
- [Retry](/docs/guides/retry)
- [Troubleshooting](/docs/troubleshooting)
