---
title: Eventual consistency
description: Why vectors are empty after INSERT, and how to wait correctly.
---

# Eventual consistency

Application writes commit first. The vector is filled later, by a worker that
does not hold your row lock while it talks to the engine.

That is the contract. Search during the window can miss a brand-new row or,
after an update, still rank the old vector. For chunked columns the contract
is stricter: an edited document is **absent** until it is rebuilt — false
negatives, never stale chunk text. See [chunking](/docs/guides/chunking).

## How to wait

```sql
SELECT relation, pending_jobs, dead_jobs, worker_last_beat
  FROM postvec.status();

SELECT count(*) FILTER (WHERE body_semantic IS NOT NULL) AS filled,
       count(*) AS total
  FROM docs;
```

Ready means `pending_jobs = 0`, `dead_jobs = 0`, and `filled = total` (or
whatever you consider complete). Presence of a heartbeat **row** is not
health — the row survives a dead worker. The timestamp must **advance**.

CLI counterpart:

```bash
sudo postvec doctor --database app --deep
```

`--deep` samples the heartbeat twice and checks it moved. `doctor` never
runs inference and never calls `refresh_models()`.

## What the worker does with failures

| Engine response | What you see |
|---|---|
| Transient / timeout | Retry with backoff (`max_retries`, `retry_backoff_ms`) |
| `CONTEXT_LENGTH_EXCEEDED` | Batch is split; one oversized row is isolated |
| Permanent / retries exhausted | Row moves to `postvec.jobs_dead` |
| Endpoints empty / engine not up yet | Jobs stay pending; attempts are not burned |

Re-drive dead letters with `retry_dead()` after you fix the cause — never
by inserting into `jobs` yourself. [Retry](/docs/guides/retry).

## Query path is synchronous

`search()` embeds the query **now**. If inference is down and
`search_degrade_to_fts` is on, you get lexical results only. If you turn
degradation off, the search errors.

`search_with_vector()` skips query embedding when you already have a vector.

## Don't

::: danger Don't poll the vector column in the same transaction as the INSERT
The worker cannot write back until you commit. Look from a new transaction.
:::
