---
title: Eventual consistency
description: Asynchronous vector updates and readiness checks.
---

# Eventual consistency

Application writes commit first. The vector is filled later, by a worker
that does not hold the application transaction's row lock while calling
the engine.

`SELECT body_semantic` inside the inserting transaction is still NULL.
Poll in a later transaction, or watch `status().pending_jobs`.

A committed write arms an **at-commit latch** and nudges the worker.
Under light load the worker normally starts within inference time of the
commit. `postvec.poll_interval_ms` (default 5000) is only the backstop:
it covers a worker that restarted between the latch being armed and the
commit.

Search during this window can miss a new row or, after an update, still
rank the previous vector. For chunked columns the contract is stricter:
an edited document is **absent** until it is rebuilt. Search can return
a false negative; it will not rank stale chunk text. See
[chunking](/docs/guides/chunking).

## Readiness checks

```sql
SELECT relation, pending_jobs, dead_jobs, worker_last_beat
  FROM postvec.status();

SELECT count(*) FILTER (WHERE body_semantic IS NOT NULL) AS filled,
       count(*) AS total
  FROM docs;
```

Ready means `pending_jobs = 0`, `dead_jobs = 0` and `filled = total`, or
the selected completeness threshold. Presence of a heartbeat **row** is
not enough. The row survives a dead worker. The timestamp must
**advance**.

CLI counterpart:

```bash
sudo postvec doctor --database app --deep
```

`--deep` samples the heartbeat twice and checks that it advanced.
`doctor` does not run inference or call `refresh_models()`.

## Failure handling

| Engine response | Result |
|---|---|
| Transient / timeout | Retry with backoff (`max_retries`, `retry_backoff_ms`) |
| `CONTEXT_LENGTH_EXCEEDED` | Batch is split; one oversized row is isolated |
| Permanent / retries exhausted | Row moves to `postvec.jobs_dead` |
| Endpoints empty / engine not up yet | Jobs stay pending; attempts are not burned |

After the underlying cause is resolved, dead letters can be re-driven
with `retry_dead()`. Direct inserts into `jobs` are unsupported. See
[Retry](/docs/guides/retry).

## Query path is synchronous

`search()` embeds the query synchronously. If inference is unavailable
and `search_degrade_to_fts` is enabled, the result is lexical only. With
degradation disabled, the search returns an error.

`search_with_vector()` skips query embedding when a vector is supplied.

## Transaction boundary

:::: danger The worker cannot fill a vector before the inserting transaction commits
Polling the vector column within the inserting transaction cannot
observe the worker write-back. Readiness checks require a separate
transaction after commit.
::::
