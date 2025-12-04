---
title: Upgrade
description: Package files, restart, and ALTER EXTENSION belong in one window.
---

# Upgrade

Two lifecycles, one maintenance window:

1. New files (`postvec.so`, control, upgrade SQL).
2. `ALTER EXTENSION postvec UPDATE` in **every** database that has it.

```bash
sudo apt install ./postgresql-18-postvec_0.2.0-1+deb12_amd64.deb
sudo systemctl restart postgresql@18-main
psql -d app -c 'ALTER EXTENSION postvec UPDATE'
sudo postvec doctor --database app --deep
```

Repeat `ALTER EXTENSION` for each database in `postvec.database`.

## Why the window matters

Between restart and `ALTER EXTENSION` the worker **parks**: no jobs, no
heartbeat. That protects worker transactions. It does **not** protect
application backends — they can load the new library against old SQL. Keep
traffic out until every database is updated.

There is no `postvec upgrade` command. This is distinct from
`postvec model upgrade`, which replaces model bytes and does not touch
extension SQL or stored vectors.

## Containers

Pull the new image, then on the **existing** volume:

```sql
ALTER EXTENSION postvec UPDATE;
```

Init scripts will not do this for you. Run `postvec-healthcheck` afterwards.

Never change the PostgreSQL major by changing the image tag against the
same volume.

## Rollback

PostgreSQL has no general extension downgrade. Restore the pre-upgrade
backup, or follow the release's rollback note if it has one.

## Don't

::: danger Don't replace only the `.so`
Same-version SQL changes on an unreleased tree are a special case for
developers. On a shipped version, missing `postvec--X--Y.sql` plus
`ALTER EXTENSION` means the worker stays parked and application calls are
undefined.
:::
