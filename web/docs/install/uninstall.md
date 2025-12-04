---
title: Uninstall
description: Remove postvec from a database, then optionally remove the packages.
---

# Uninstall

Choose the layer that matches the end state. Never
`DROP EXTENSION postvec CASCADE`.

| Goal | Command |
|---|---|
| Preview | `sudo postvec uninstall --database app --dry-run` |
| Remove postvec from one database, keep data | `sudo postvec uninstall --database app` |
| Also drop postvec-created shadow columns | add `--drop-columns --acknowledge-data-loss --yes` |
| Drop a chunk destination | SQL `disable(..., drop_destination => true)` **first** |
| Reset a disposable DB and retry `setup` | [below](#reset-and-retry) |
| Remove packages after SQL cleanup | `sudo apt remove postgresql-18-postvec postvec-cli` |

## Default is retain

`uninstall` runs `postvec.uninstall(...)` and `DROP EXTENSION postvec`
**without** `CASCADE` in one transaction, then removes that database from
the CLI-owned config.

| Removed | Kept |
|---|---|
| Triggers, jobs, migrations, the extension | The database itself |
| The name in `99-postvec.conf` | pgvector, source tables, source data |
| Shadow columns, only with `--drop-columns` | Adopted / user-owned vector columns |
| — | Chunk destinations and their views |

There is **no** `--drop-destinations` on the CLI. Destinations stay as
ordinary tables after this command.

```sql
-- before CLI uninstall, if you want the chunks gone:
SELECT postvec.disable('public.articles', 'body', drop_destination => true);
```

## Reset and retry (dev)

Packages and engine assets stay installed.

```bash
sudo postvec uninstall --database app --yes
# The worker holds a connection; a plain DROP DATABASE fails.
sudo -u postgres dropdb --force --if-exists app

sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference --yes
sudo postvec doctor --database app --deep
```

`uninstall` takes the name out of the running launcher (restart). If you
`dropdb` *without* uninstalling first, rerun `uninstall` — or edit
`postvec.database` **and restart**. Reload is not enough: the list is
POSTMASTER. Until the name is gone, the worker FATAL-loops every ~15 s.

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

Then remove packages, if that is the end state:

```bash
sudo apt remove postgresql-18-postvec postvec-cli
sudo apt remove postvec-embedded postvec-model-minilm-l6-v2 \
  postvec-onnxruntime
```

On EL9, `dnf remove` the corresponding names (`postgresql18-postvec`, …).

## Exit 3

SQL went through but the CLI would not touch configuration (hand-owned or
drifted file). It prints the file and line. Follow that — a launcher may
still be connecting to the removed database. `--keep-config` makes the
same state explicit (required for a URI-only target).

## Docker

```bash
docker rm -f postvec
docker volume rm postvec-data     # only if you want the data gone
```

## Don't

::: danger Don't `DROP EXTENSION … CASCADE`
That walks unknown dependent objects. The CLI and the SQL `uninstall()`
exist so teardown is bounded.
:::

::: danger Don't `dropdb` while the worker is configured
Use `uninstall` first, then `dropdb --force`. Otherwise the launcher
respawns a worker against a missing database.
:::
