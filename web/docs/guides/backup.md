---
title: Backup and restore
description: Data included in pg_dump and the post-restore checklist.
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

## Post-restore procedure

1. Install the **matching** postvec and pgvector files for that major.
2. Restore cluster configuration (`99-postvec.conf` or its equivalent)
   and restart so the launcher and workers start.
3. `SELECT postvec.refresh_models();` or run `postvec setup`.
4. `postvec doctor --database … --deep`.
5. Confirm queues and any open migration before reopening writes.

Restored queued and dead **chunk** work resumes against the restored
chunk identities. Destination sequences are therefore part of the dump.

The restored cluster must load a version-compatible `postvec.so`. Triggers
cannot run when the library is missing.

## Logical replication

Statement triggers do not fire for subscriber-applied changes. Run
postvec on the **publisher**, or use `trigger_mode => 'row'` and
understand that the subscriber still needs its own worker and models if
independent embedding is required on the subscriber.

## Configuration constraint

::: danger `postvec.database` must match the restored databases
Missing databases cause workers to repeatedly fail during connection.
The list must be edited, or postvec uninstalled, followed by a restart before
the cluster is opened for application traffic.
:::
