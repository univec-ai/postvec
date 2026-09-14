---
title: Uninstall
description: Database teardown, chunk destinations and host cleanup for PostgreSQL 16, 17 and 18.
---

# Uninstall

How far removal goes depends on the required end state. Preview first
with `--dry-run`. The CLI runs `postvec.uninstall(...)` and then
`DROP EXTENSION postvec` without `CASCADE` in one transaction. A
SQL-only teardown runs `postvec.uninstall(...)` first and drops the
extension afterwards.

| Goal | Command |
|---|---|
| Preview | `sudo postvec uninstall --database app --dry-run` |
| Remove postvec from one database, keep data | `sudo postvec uninstall --database app` |
| Also drop postvec-created shadow columns | add `--drop-columns --acknowledge-data-loss --yes` |
| Keep chunk destinations as frozen tables | add `--drop-columns --keep-destinations` |
| Remove postvec from every database in the cluster | `sudo postvec uninstall --all ...` |
| Clear postvec's files from the host (experimental) | add `--purge --yes` |
| Drop a chunk destination in SQL | `disable(..., drop_destination => true)` **first** |
| Reset a development database and retry `setup` | [development reset](#development-reset) |
| Remove packages after SQL cleanup | [package removal](#package-removal) below |

## What the command removes

`uninstall` runs `postvec.uninstall(...)` and `DROP EXTENSION postvec`
**without** `CASCADE` in one transaction, then removes that database
from the CLI-owned config.

| Removed | Kept |
|---|---|
| Triggers, jobs, migrations, the extension | The database itself |
| The name in `99-postvec.conf` | pgvector, source tables, source data |
| Shadow columns, with `--drop-columns` | Adopted / user-owned vector columns |
| Managed chunk destination tables and views, with `--drop-columns` | Chunk destinations, with `--drop-columns --keep-destinations` |
| | Models under `/opt/postvec`; `--purge` removes the ones postvec installed |
| | [Provider connector files](/docs/models/providers) and their keys |

`--drop-columns` drops the shadow vector columns **and**, for chunked
entries, the managed chunk destination tables and views. Each
destination goes only after its ownership marker proves postvec created
it. `--keep-destinations` restricts the command to the shadow columns
and leaves the destinations as ordinary frozen tables. While the
extension is still installed, drop one of those tables in SQL:

```sql
SELECT postvec.disable('public.articles', 'body', drop_destination => true);
```

## Selecting databases

`--database NAME` is repeatable and accepts a comma-separated list.
`--all` covers every database the launcher is configured to serve (the
live setting, the owned file and the CLI state) plus any other database
in the cluster where the extension is installed. It inventories every
`pg_database` row, `template1` included; `template0` is exempt.

In a non-interactive run, `--drop-columns` also needs
`--acknowledge-data-loss`; at a terminal you can type the database names
to confirm instead. `--yes` alone never authorizes data loss. A database
the command cannot inspect is reported and skipped, and the result is
partial (exit 3).

## Provider credentials

When the last configured database goes away, `uninstall` reports the
[provider connector files](/docs/models/providers) left in `providers.d`
and any key files beside them, and leaves both on disk:

```text
postvec: /etc/postvec/providers.d still holds 1 provider connector file(s) with API credentials
```

The line names the key files it found and states that they were not
removed. Package removal leaves them too and warns about the same
directory. They hold API credentials, so delete them by hand once no host
still needs them:

```bash
sudo rm -r /etc/postvec/providers.d
```

## Development reset

Packages and engine assets stay installed.

```bash
sudo postvec uninstall --database app --yes
# The worker holds a connection; a plain DROP DATABASE fails.
sudo -u postgres dropdb --force --if-exists app

sudo postvec setup --database app \
  --embedded --yes
sudo postvec doctor --database app --deep
```

`uninstall` takes the name out of the running launcher through a
restart. If `dropdb` ran first, rerun `uninstall` or edit
`postvec.database` **and restart**: the setting is read when a worker
starts, and with a preloaded launcher that means the next PostgreSQL
restart. Until the name is removed, the worker fails at startup and the
launcher respawns it on a ladder of 15, 30, 60, 120, 240 and 300 seconds,
capped at 300 seconds. A worker that stays up for 30 seconds resets the
ladder, so a database created later recovers without a restart.

## SQL-only teardown

:::: code-group

```sql [SQL]
SELECT postvec.disable('public.docs', 'body');
SELECT postvec.uninstall();                 -- superuser; keeps columns
SELECT postvec.uninstall(
  drop_columns => true,
  drop_destinations => true
);
DROP EXTENSION postvec;                     -- no CASCADE
```

```bash [CLI]
sudo postvec uninstall --database app --dry-run
sudo postvec uninstall --database app
sudo postvec uninstall --database app \
  --drop-columns --acknowledge-data-loss --yes
```

::::

## Package removal

After database teardown, packages may also be removed. User data stays
in the database.

<PgSnippet id="uninstall-packages" />

## Host cleanup with `--purge`

:::: danger Experimental
`--purge` is destructive cleanup for disposable hosts and not a
production promise. Ordinary `uninstall --all` and `--drop-columns` do
not depend on it.
::::

`--purge` requires `--all` and conflicts with `--keep-config` and
`--no-restart`. After the SQL and configuration removal, it stops the
cluster, deletes the files postvec can positively attribute to itself,
and starts the cluster again.

| Deleted, with evidence | Retained |
|---|---|
| Model trees under the engine root that carry the engine descriptor and hold no package-owned file | A tree without the descriptor, or one whose contents could not be read |
| Staging and trash directories under `models/`, and an unpackaged `libs/` holding ONNX Runtime | A package-owned `libs/` |
| Connector `.toml` files the CLI wrote, credentials included | A key file the operator provided |
| The CLI state file under `/var/lib/postvec/clusters` and the registry login `/var/lib/postvec/auth.json` | A per-user `~/.config/postvec/auth.json` |
| An unpackaged `postvec.so`, `postvec.control` and `postvec--*.sql` | Those files when a package owns them, when postvec is preloaded from a file the CLI does not own, or when another cluster exists |

Package-owned files are never deleted; the packages to purge are printed
instead, as one package-manager removal line. Anything in doubt stays on
disk and makes the result partial (exit 3). Everything else under the
engine root is retained, including a `providers.d` that belongs to a
[postvec-server](/docs/server/) node. Files outside those paths, such as
`/etc/postvec-server` and the node's certificate pair, are not touched.

`--purge` refuses to run when:

- another cluster on the host still uses postvec, or its configuration cannot be read;
- a database cannot be inspected, or still records the extension;
- postvec is still configured after the restart, or the offline configuration is not clear;
- the cluster cannot be stopped, or the host has no service command for it (an explicit `--pg-config` target).

It supports clusters that `pg_lsclusters` lists. A running
`postvec-server` process does not refuse the run; the engine root is left
alone with a note.

## Exit 3

Exit code 3 means the teardown was partial:

| Cause | Detail |
|---|---|
| A chunk destination was retained | The server refused to drop it because its ownership marker or structure no longer proves postvec created it. It is an ordinary table now |
| A database could not be inspected | `--all` only. postvec may still be installed there |
| Configuration was left unchanged | SQL removal succeeded but the file is hand-owned or drifted; the diagnostic names the file and line |

`--keep-config` produces the configuration state explicitly and is
required for a URI-only target. After a partial result a launcher may
still be connecting to the removed database.

## Docker

```bash
docker rm -f postvec
docker volume rm postvec-data     # only when permanent data removal is required
```

## Destructive operations

:::: danger Use `uninstall()` then `DROP EXTENSION` without `CASCADE`
`CASCADE` can remove unknown dependent objects. The CLI and the SQL
`uninstall()` provide bounded teardown.
::::

:::: danger Remove worker configuration before `dropdb`
Run `uninstall` before `dropdb --force`. Otherwise the launcher
repeatedly respawns a worker against a missing database.
::::
