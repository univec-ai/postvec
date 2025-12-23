---
title: Upgrade
description: Coordinating package files, PostgreSQL restart and ALTER EXTENSION for PostgreSQL 16, 17 and 18.
---

# Upgrade

Two lifecycles share one maintenance window:

1. New files (`postvec.so`, control, upgrade SQL).
2. `ALTER EXTENSION postvec UPDATE` in **every** database that has it.

Replace `NEWVERSION` with the version being installed.

<PgSnippet id="upgrade-packages" />

Repeat `ALTER EXTENSION` for each database in `postvec.database`.

## Maintenance window

Between restart and `ALTER EXTENSION` the worker **pauses**: no jobs, no
heartbeat. Worker transactions therefore cannot run against mismatched
SQL. Application backends can still load the new library against old
SQL, so application traffic should stay paused until every database is
updated.

Upgrade the packages, restart, then run `ALTER EXTENSION`.
`postvec model upgrade` replaces model bytes only.

## Containers

Pull the new image, then on the **existing** volume:

```sql
ALTER EXTENSION postvec UPDATE;
```

Init scripts do not apply extension upgrades to existing volumes. Run
`postvec-healthcheck` afterward.

A PostgreSQL major change requires `pg_upgrade` or dump/restore;
changing only the image tag against the same volume is unsupported.

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
