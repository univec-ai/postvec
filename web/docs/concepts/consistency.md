---
title: Eventual consistency
description: Asynchronous vector updates and readiness checks.
---

# Eventual consistency

Application writes commit first. A background worker then fills the
vector in its own session.

`SELECT body_semantic` inside the inserting transaction is still NULL.
Poll in a later transaction, or watch `status().pending_jobs`.

A commit wakes the worker, so under light load the vector usually
appears as soon as inference finishes. If the worker was restarting at
that moment, it picks the job up on its next poll
(`postvec.poll_interval_ms`, default 5 seconds).

Search during this window can miss a new row or, after an update, still
rank the previous vector. For chunked columns the contract is stricter:
an edited document is **absent** until it is rebuilt, so ranks use only
current chunk text. See [chunking](/docs/guides/chunking).

## Readiness checks

```sql
SELECT relation, pending_jobs, dead_jobs, worker_last_beat
  FROM postvec.status();

SELECT count(*) FILTER (WHERE body_semantic IS NOT NULL) AS filled,
       count(*) AS total
  FROM docs;
```

Ready means `pending_jobs = 0`, `dead_jobs = 0` and `filled = total`, or
the selected completeness threshold. Health requires the heartbeat
timestamp to **advance** between samples. The row itself survives a
dead worker.

CLI counterpart:

```bash
sudo postvec doctor --database app --deep
```

`--deep` samples the heartbeat twice and checks that it advanced.
`doctor` is read-only: host files, cluster settings and the heartbeat.

## Failure handling

| Engine response | Result |
|---|---|
| Transient / timeout | Retry with backoff (`max_retries`, `retry_backoff_ms`) |
| `CONTEXT_LENGTH_EXCEEDED` | Batch is split; one oversized row is isolated |
| Permanent / retries exhausted | Row moves to `postvec.jobs_dead` |
| Endpoints empty / engine not up yet | Jobs stay pending; retry counts stay |
| One fleet node lacks the model or converter | Intermittent failures; jobs can stay pending |

After the underlying cause is resolved, dead letters can be re-driven
with `retry_dead()`. Direct inserts into `jobs` are unsupported. See
[Retry](/docs/guides/retry).

Remote mode reaches inference over a node list served by
[postvec-server](/docs/server/). When the inventories drift, a node
without the model or converter fails once endpoint rotation reaches it,
so jobs fail intermittently or stay pending.
`postvec-server status --fleet` names the model and the nodes missing it.

## Query path is synchronous

`search()` embeds the query synchronously. If inference is unavailable
and `search_degrade_to_fts` is enabled, the result is lexical only. With
degradation disabled, the search returns an error.

`search_with_vector()` skips query embedding when a vector is supplied.

## Transaction boundary

:::: danger Readiness is only visible in a later transaction
A read inside the inserting transaction returns NULL or the previous
vector. Poll after commit.
::::
