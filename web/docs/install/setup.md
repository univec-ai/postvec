---
title: Configure the cluster
description: postvec setup and doctor — the only commands that change a cluster.
---

# Configure the cluster

Files are installed. This page turns them into a running worker.

`setup` creates missing databases, installs the extension, merges
`shared_preload_libraries`, writes **one** owned file
(`conf.d/99-postvec.conf`), validates it, restarts or reloads if needed,
refreshes models, and proves the worker heartbeat **advances**.

Always preview:

```bash
sudo postvec setup --database app ... --dry-run
```

## Embedded (start here)

This is the on-prem path: engine in the launcher, no account, no
outbound embed API.

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference

sudo postvec model ls
sudo postvec doctor --database app --deep
```

::: tip Expected
`model ls` shows MiniLM as loaded. `doctor` reports aligned library/SQL
versions, an advancing heartbeat, and matching on-disk / engine / SQL
inventories.
:::

`--model NAME` (repeatable) is an embedded preload allow-list. Omit it to
scan-load every enabled descriptor.

## Remote — organisations

ninference on your network: distributed CPU/GPU inference, the private
catalogue, support. Same SQL. See [embedded vs remote](/docs/concepts/modes).

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222

sudo postvec doctor --database app --deep
```

`--allow-unreachable` is only for staging configuration before ninference
exists. The worker will still be up; inference will not.

## EL9 / non-`postgresql-common`

There is no `pg_lsclusters`. Select the binaries and a directory
`postgresql.conf` already includes. The CLI will not invent that
relationship.

```bash
sudo -u postgres postvec setup \
  --pg-config /usr/pgsql-18/bin/pg_config \
  --config-dir /path/included/by/postgresql.conf \
  --database app \
  --embedded --path /opt/postvec/ninference \
  --no-restart

sudo systemctl restart postgresql-18.service
sudo -u postgres postvec doctor \
  --pg-config /usr/pgsql-18/bin/pg_config \
  --database app --deep
```

Run as the cluster owner, or set `POSTVEC_DATABASE_URL`. `--no-restart`
writes valid state and exits **4**; you restart, then `doctor`.

## Several databases

`postvec.database` is cluster-wide. Each `--database` **adds**; it does not
remove the others.

```bash
sudo postvec setup --database analytics \
  --embedded --path /opt/postvec/ninference
```

Switching remote ↔ embedded requires `--switch-mode` and must name **every**
configured database.

Never hand-edit a name that does not exist into `postvec.database`. The
launcher will FATAL-respawn that worker forever. `setup` creates the
database *before* activating the list so this cannot happen.

## What `setup` will refuse

| State of `99-postvec.conf` | Result |
|---|---|
| Missing / CLI-owned and unchanged | Writable |
| Hand-written (`Foreign`) | Refuse |
| CLI-owned but edited (`Modified`) | Refuse |

`--yes` does **not** override those two. Reconcile or move the file.

Do not mix a hand-owned `postvec.conf` with the CLI-owned `99-postvec.conf`.

## Prove it from SQL

```sql
SELECT postvec.version(), postvec.build_info();
SELECT extname, extversion FROM pg_extension
 WHERE extname IN ('vector', 'postvec');
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
SELECT * FROM postvec.status();
```

::: tip Expected
Both extensions present. At least one model if inference is reachable.
`worker_last_beat` is recent and moves if you wait and look again.
:::

## Manual configuration (you own it)

Only if you deliberately do not want the CLI to own the file:

```ini
shared_preload_libraries = 'postvec'
postvec.database = 'app'
postvec.mode = 'embedded'
postvec.ninference_path = '/opt/postvec/ninference'
```

Remote organisations instead set `postvec.mode = 'grpc'` and the two
endpoint GUCs. Prefer `setup` over hand-editing.

Create the database and `CREATE EXTENSION postvec CASCADE` **before** adding
the name to `postvec.database`, then restart.

## `doctor` in one paragraph

Read-only. ~40 checks, each with a fix. `--deep` proves the heartbeat
advanced and hashes CLI-installed model receipts. `--strict` fails on
warnings. `--format json` is the automation surface. Exit 0 is clean.

```bash
sudo postvec doctor --database app --deep --format json \
  | jq '{summary, failures: [.checks[] | select(.status == "FAIL")]}'
```

Full flag list: [CLI reference](/docs/reference/cli).
