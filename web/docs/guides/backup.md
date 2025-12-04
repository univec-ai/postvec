---
title: Backup and restore
description: What pg_dump carries, what it doesn't, and the post-restore checklist.
---

# Backup and restore

`pg_dump` carries the durable control tables — `registry`, `jobs`,
`jobs_dead`, `migrations` — and their identity sequences. Generated
triggers dump as ordinary database objects. A chunked destination, its
identity sequence, join view, ownership-marker comments, RLS policy, and
triggers dump normally.

It does **not** dump:

- `postvec.models` (discovery cache)
- the worker heartbeat

## After restore

1. Install the **matching** postvec and pgvector files for that major.
2. Restore cluster configuration (`99-postvec.conf` or your equivalent)
   and restart so the launcher and workers start.
3. `SELECT postvec.refresh_models();` or run `postvec setup`.
4. `postvec doctor --database … --deep`.
5. Confirm queues and any open migration before reopening writes.

Restored queued and dead **chunk** work resumes against the restored
chunk identities. That is why destination sequences are part of the dump.

The restored cluster must still load a version-compatible `postvec.so`.
Triggers pointing at a missing library will not save you.

## Logical replication

Statement triggers do not fire for subscriber-applied changes. Run
postvec on the **publisher**, or use `trigger_mode => 'row'` and
understand that the subscriber still needs its own worker and models if
you expect it to embed independently.

## Don't

::: danger Don't restore into a cluster whose `postvec.database` still
names databases you did not restore
Workers FATAL-loop on connect. Uninstall or edit the list **and restart**
before opening the cluster.
:::
