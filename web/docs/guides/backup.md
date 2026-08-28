---
title: Backup and restore
description: Data included in pg_dump and the post-restore checklist.
---

# Backup and restore

`pg_dump` carries the durable control tables (`registry`, `jobs`,
`jobs_dead`, `migrations`) and their identity sequences. Generated
triggers dump as ordinary database objects. A chunked destination, its
identity sequence, join view, ownership-marker comments, RLS policy
and triggers dump normally.

Caches rebuilt after restore:

- BM25 corpus statistics (`lexical_stats`, `lexical_df`): the worker rebuilds
  them at its first wake; searches use `ts_rank_cd` until then
- `postvec.models` (discovery cache)
- the worker heartbeat

## Post-restore procedure

1. Install the **matching** postvec and pgvector files for that major.
2. Restore cluster configuration (`99-postvec.conf` or its equivalent) and restart so the launcher and workers start.
3. `SELECT postvec.refresh_models();` or run `postvec setup`.
4. `postvec doctor --database ... --deep`.
5. Confirm queues and any open migration before reopening writes.

Restored queued and dead **chunk** work resumes against the restored chunk identities. Destination sequences are therefore part of the dump.

The restored cluster must load a version-compatible `postvec.so`. Triggers cannot run when the library is missing.

## Logical replication

Statement triggers fire on the publisher. Subscriber-applied changes skip them. Run postvec on the **publisher**, or use `trigger_mode => 'row'` and give the subscriber its own worker and models if it should embed independently.

## Configuration constraint

:::: danger `postvec.database` must match the restored databases
Missing databases cause workers to repeatedly fail during connection.
The list must be edited, or postvec uninstalled, followed by a restart before
the cluster is opened for application traffic.
::::
