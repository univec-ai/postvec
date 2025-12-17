---
title: Configure the cluster
description: Cluster configuration and diagnostics with postvec setup and doctor.
---

# Configure the cluster

File installation stops at the binaries. `postvec setup` then configures
the launcher, the database workers and the inference mode.

`setup` creates missing databases, installs the extension, merges
`shared_preload_libraries`, writes **one** owned file
(`conf.d/99-postvec.conf`), validates it, restarts or reloads if needed,
refreshes models and verifies that the worker heartbeat **advances**.

A dry run reports the planned changes without applying them:

```bash
sudo postvec setup --database app ... --dry-run
```

## Embedded

The engine runs in the launcher. No account or outbound embedding API is
required.

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference

sudo postvec model ls
sudo postvec doctor --database app --deep
```

:::: tip Expected
`model ls` shows MiniLM as loaded. `doctor` reports aligned library/SQL
versions, an advancing heartbeat and matching on-disk / engine / SQL
inventories.
::::

`--model NAME` (repeatable) is an embedded preload allow-list. Omit it to
scan-load every enabled descriptor.

## Remote gRPC

Remote mode uses ninference nodes on the local network for distributed CPU or
GPU inference and private catalogue access. The SQL surface is unchanged. See
[embedded vs remote](/docs/concepts/modes).

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222

sudo postvec doctor --database app --deep
```

`--allow-unreachable` is only for staging configuration before ninference
exists. The worker starts, but inference remains unavailable.

## EL9 / non-`postgresql-common`

There is no `pg_lsclusters`. `--pg-config` must identify the PostgreSQL
binaries and `--config-dir` must already be included by `postgresql.conf`.

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
writes valid state and exits **4**. PostgreSQL must then be restarted before
running `doctor`.

## Multiple databases

`postvec.database` is cluster-wide. Each `--database` **adds**; it does not
remove the others.

```bash
sudo postvec setup --database analytics \
  --embedded --path /opt/postvec/ninference
```

Switching between remote and embedded requires `--switch-mode` and must name
**every** configured database.

Every name in `postvec.database` must refer to an existing database. A missing
database causes the launcher to repeatedly respawn the failing worker. `setup`
creates the database before activating the list.

## Configuration ownership checks

| State of `99-postvec.conf` | Result |
|---|---|
| Missing / CLI-owned and unchanged | Writable |
| Hand-written (`Foreign`) | Refuse |
| CLI-owned but edited (`Modified`) | Refuse |

`--yes` does **not** override those two. Reconcile or move the file.

A manually maintained `postvec.conf` should not coexist with the CLI-owned
`99-postvec.conf`.

## SQL verification

```sql
SELECT postvec.version(), postvec.build_info();
SELECT extname, extversion FROM pg_extension
 WHERE extname IN ('vector', 'postvec');
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
SELECT * FROM postvec.status();
```

:::: tip Expected
Both extensions are present. At least one model is listed when inference is
reachable.
`worker_last_beat` is recent and advances between samples.
::::

## Manual configuration

Manual configuration keeps the file outside CLI ownership:

```ini
shared_preload_libraries = 'postvec'
postvec.database = 'app'
postvec.mode = 'embedded'
postvec.ninference_path = '/opt/postvec/ninference'
```

Remote deployments set `postvec.mode = 'grpc'` and the two endpoint GUCs.
`setup` is preferred over manual editing.

Create the database and `CREATE EXTENSION postvec CASCADE` **before** adding
the name to `postvec.database`, then restart.

## `doctor` summary

`doctor` is read-only and performs approximately 40 checks, each with a
remediation. `--deep` verifies that the heartbeat advanced and hashes
CLI-installed model receipts. `--strict` fails on
warnings. `--format json` is the automation surface. Exit 0 is clean.

```bash
sudo postvec doctor --database app --deep --format json \
  | jq '{summary, failures: [.checks[] | select(.status == "FAIL")]}'
```

Full flag list: [CLI reference](/docs/reference/cli).
