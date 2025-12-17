---
title: Uninstall
description: Database cleanup and optional package removal.
---

# Uninstall

How far removal goes depends on the required end state. `DROP EXTENSION
postvec CASCADE` is unsupported.

| Goal | Command |
|---|---|
| Preview | `sudo postvec uninstall --database app --dry-run` |
| Remove postvec from one database, keep data | `sudo postvec uninstall --database app` |
| Also drop postvec-created shadow columns | add `--drop-columns --acknowledge-data-loss --yes` |
| Drop a chunk destination | SQL `disable(..., drop_destination => true)` **first** |
| Reset a disposable DB and retry `setup` | [development reset](#development-reset) |
| Remove packages after SQL cleanup | `sudo apt remove postgresql-18-postvec postvec-cli` |

## Default retention behavior

`uninstall` runs `postvec.uninstall(...)` and `DROP EXTENSION postvec`
**without** `CASCADE` in one transaction, then removes that database from
the CLI-owned config.

| Removed | Kept |
|---|---|
| Triggers, jobs, migrations, the extension | The database itself |
| The name in `99-postvec.conf` | pgvector, source tables, source data |
| Shadow columns, only with `--drop-columns` | Adopted / user-owned vector columns |
| | Chunk destinations and their views |

There is **no** `--drop-destinations` on the CLI. Destinations stay as
ordinary tables after this command.

```sql
-- before CLI uninstall, when chunk removal is required:
SELECT postvec.disable('public.articles', 'body', drop_destination => true);
```

## Development reset

Packages and engine assets stay installed.

```bash
sudo postvec uninstall --database app --yes
# The worker holds a connection; a plain DROP DATABASE fails.
sudo -u postgres dropdb --force --if-exists app

sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference --yes
sudo postvec doctor --database app --deep
```

`uninstall` takes the name out of the running launcher through a restart.
If `dropdb` ran first, rerun `uninstall` or edit `postvec.database` **and
restart**. Reload is not enough: the list is POSTMASTER. Until the name is
removed, the worker fails and respawns approximately every 15 seconds.

## SQL-only teardown

```sql
SELECT postvec.disable('public.docs', 'body');
SELECT postvec.uninstall();                 -- superuser; keeps columns
SELECT postvec.uninstall(
  drop_columns => true,
  drop_destinations => true
);
DROP EXTENSION postvec;                     -- no CASCADE
```

After database teardown, packages may also be removed:

```bash
sudo apt remove postgresql-18-postvec postvec-cli
sudo apt remove postvec-embedded postvec-model-minilm-l6-v2 \
  postvec-onnxruntime
```

On EL9, `dnf remove` the corresponding names (`postgresql18-postvec`, ...).

## Exit 3

Exit code 3 means SQL teardown finished, but the CLI left a hand-owned or
drifted configuration file untouched. The diagnostic names the file and
line. A launcher may still be connecting to the removed database.
`--keep-config` produces the same state explicitly and is required for a
URI-only target.

## Docker

```bash
docker rm -f postvec
docker volume rm postvec-data     # only when permanent data removal is required
```

## Destructive operations

:::: danger `DROP EXTENSION ... CASCADE` is unsupported
`CASCADE` can remove unknown dependent objects. The CLI and the SQL
`uninstall()` provide bounded teardown.
::::

:::: danger Remove worker configuration before `dropdb`
Run `uninstall` before `dropdb --force`. Otherwise the launcher repeatedly
respawns a worker against a missing database.
::::
