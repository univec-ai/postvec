---
title: Status and health
description: status(), stats(), and postvec doctor — what healthy looks like.
---

# Status and health

Two SQL views of one worker, plus a read-only CLI that can see the host.

## `status()` — per entry

```sql
SELECT relation, model, dim, state,
       pending_jobs, dead_jobs,
       has_vector_index, index_error,
       worker_last_beat, last_error,
       chunking, pending_refresh_jobs, pending_embed_jobs
  FROM postvec.status();
```

| You want | Look at |
|---|---|
| Backfill done | `pending_jobs = 0` (and refresh/embed = 0 if chunked) |
| Failures | `dead_jobs`, `last_error` |
| Worker alive | `worker_last_beat` **advances** |
| Search will be fast | `has_vector_index` |
| Auto-index parked | `index_error` |
| Quarantined entry | `state` (source/vector/template column vanished) |

A heartbeat **row** surviving a crash is not health. Sample twice.

## `stats()` — the worker

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

`doctor` never runs inference, never calls `refresh_models()`, and has no
mutating database handle.

Inside a container, use `postvec-healthcheck` or an explicit socket URL —
[Docker](/docs/install/docker).

## Version

```sql
SELECT postvec.version();
SELECT postvec.build_info();  -- {version, diagnostics_api, features.embedded}
```

If `doctor` (or the worker log) asks for `ALTER EXTENSION`, the library
and installed SQL disagree. [Upgrade](/docs/install/upgrade).
