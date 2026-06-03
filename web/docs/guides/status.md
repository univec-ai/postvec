---
title: Status and health
description: Health signals from status(), stats() and postvec doctor.
---

# Status and health

`status()` is per enabled column. `stats()` is the worker process.
`postvec doctor` adds read-only checks of the host installation.

Start here when vectors stay NULL, search is lexical-only or a
migration looks stuck.

## `status()` - per entry

:::: code-group

```sql [SQL]
SELECT relation, model, dim, state,
       pending_jobs, dead_jobs,
       has_vector_index, index_error,
       worker_last_beat, last_error,
       chunking, pending_refresh_jobs, pending_embed_jobs
  FROM postvec.status();
```

```bash [CLI]
sudo postvec doctor --database app --deep
```

::::

| Check | Field |
|---|---|
| Backfill done | `pending_jobs = 0` (and refresh/embed = 0 if chunked) |
| Failures | `dead_jobs`, `last_error` |
| Worker alive | `worker_last_beat` **advances** |
| Search will be fast | `has_vector_index` |
| Automatic index build paused after failure | `index_error` |
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

## `doctor`

```bash
sudo postvec doctor --database app --deep
sudo postvec doctor --database app --deep --format json \
  | jq '{summary, failures: [.checks[] | select(.status == "FAIL")]}'
```

| Flag | Effect |
|---|---|
| `--deep` | Heartbeat must advance; hash CLI-installed model receipts |
| `--strict` | Warnings fail the command |
| `--format json` | Versioned object on stdout; progress on stderr |

`doctor` is read-only: host files, cluster settings and the heartbeat.

Inside a container, use `postvec-healthcheck` or an explicit socket URL. See [Docker](/docs/install/docker).

## Version

```sql
SELECT postvec.version();
SELECT postvec.build_info();  -- {version, diagnostics_api, features.embedded}
```

If `doctor` (or the worker log) asks for `ALTER EXTENSION`, the library and installed SQL disagree. See [Upgrade](/docs/install/upgrade).
