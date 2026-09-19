---
title: Upgrade
description: Coordinating package files, PostgreSQL restart and ALTER EXTENSION for PostgreSQL 16, 17 and 18.
---

# Upgrade

Two lifecycles share one maintenance window:

1. New files (`postvec.so`, control, upgrade SQL).
2. `ALTER EXTENSION postvec UPDATE` in **every** database that has it.

Do both in that order. Replacing the library alone parks the worker
until the SQL catches up.

Model bytes are separate: `postvec model upgrade NAME...` replaces them in
place, preserves each model's activation state, and `--all` skips
withdrawn roots. `model pull` never replaces an installed name.

The filenames are for the current release. Download them from
[Release artifacts](/download) first.

<PgSnippet id="upgrade-packages" />

Repeat `ALTER EXTENSION` for each database in `postvec.database`.

## Maintenance window

Between restart and `ALTER EXTENSION` the worker **pauses**: no jobs, no
heartbeat. Worker transactions therefore cannot run against mismatched
SQL. Application backends can still load the new library against old
SQL, so application traffic should stay paused until every database is
updated.

Upgrade the packages, restart, then run `ALTER EXTENSION`.

## Containers

Pull the new image, then on the **existing** volume:

```sql
ALTER EXTENSION postvec UPDATE;
```

Init scripts apply only on first initialization of an empty volume.
Run `ALTER EXTENSION` and `postvec-healthcheck` afterward.

A PostgreSQL major change requires `pg_upgrade` or dump/restore;
changing only the image tag against the same volume is unsupported.

## Inference nodes

[postvec-server](/docs/server/) nodes are built from the same release and
speak the extension's wire contract. A remote-mode upgrade therefore
installs the matching node package on every node in the same window:
restart one node at a time and wait for `/ready` to return 200 before the
next ([fleet](/docs/server/fleet)). `postvec-server status` prints the
version of every member, which names a node that was skipped.

## Rollback

PostgreSQL has no general extension downgrade. Restore the pre-upgrade
backup, or follow the release's rollback note if it has one.

## Version alignment

:::: danger Shipped upgrades require matching library and SQL changes
Same-version SQL changes on an unreleased tree are a special case for
developers. On a shipped version, missing `postvec--X--Y.sql` plus
`ALTER EXTENSION` means the worker remains paused and application calls
are undefined.
::::
