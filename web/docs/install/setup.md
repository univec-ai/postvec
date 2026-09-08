---
title: Configure the cluster
description: Cluster configuration and diagnostics with postvec setup and doctor, for PostgreSQL 16, 17 and 18.
---

# Configure the cluster

`postvec setup` writes cluster configuration, creates the extension and
starts the worker. Run it after [packages](/docs/install/packages),
[Docker](/docs/install/docker) or a [source](/docs/install/source) copy.

RDS, Aurora, Cloud SQL, Azure, Supabase and Neon use
[managed PostgreSQL](/docs/server/managed) instead: the worker runs
inside [postvec-server](/docs/server/).

The command creates missing databases, installs the extension, merges
`shared_preload_libraries`, writes `conf.d/99-postvec.conf`, validates
it, restarts or reloads if a setting changed, refreshes models and
checks that the worker heartbeat **advances**.

Where postvec is not preloaded yet, `setup` also starts the worker in each
database immediately with `postvec.start_worker()`, so the database is served
before the restart. With `--no-restart` the worker runs now and the restart,
which makes it survive server restarts, can happen later. The same function
works by hand after a plain `CREATE EXTENSION postvec`: run
`SELECT postvec.start_worker()` as a superuser.

:::: info Optional
A dry run reports the planned changes without applying them:

```bash
sudo postvec setup --database app ... --dry-run
```
::::

## Embedded

The engine runs in the launcher. This is the extension's **default**
mode. `postvec.path` defaults to `/opt/postvec`, where the engine-asset
packages install, so `setup --embedded` needs no `--path` on a package
install.

`setup` enrols the database in `postvec.database`, installs the
extension and restarts.

<PgSnippet id="setup-embedded" />

:::: tip Expected
`model ls` shows MiniLM as loaded.
::::

`--model NAME` (repeatable) is an embedded preload allow-list. Omit it
to load every enabled model.

`--providers-path DIR` moves the [external provider](/docs/models/providers)
connector directory. Omit it to keep `/etc/postvec/providers.d`. `setup
--embedded` creates that directory empty (`0700`, cluster owner) if it is
absent.

## Remote gRPC (postvec-server)

Remote mode uses [postvec-server](/docs/server/) on the local network.
Typical reasons: GPU inference, a separate process from PostgreSQL,
multi-threaded inference on the same VM, one engine shared by several
databases, model management from the dashboard. SQL is unchanged. See
[when to use postvec-server](/docs/server/usage),
[embedded vs remote](/docs/concepts/modes) and
[connect PostgreSQL](/docs/server/connect).

Every node must carry the same enabled models: postvec round-robins the
configured endpoints, so a converter present on two nodes out of three
fails one request in three.

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222
```

`--allow-unreachable` is only for staging configuration before
a node exists. The worker starts, but inference remains unavailable.

## Selecting the cluster

When more than one PostgreSQL major is installed, name the cluster
explicitly. EL9 has no `pg_lsclusters`; `--pg-config` must identify the
binaries and `--config-dir` must already be included by
`postgresql.conf`.

<PgSnippet id="setup-embedded-cluster" />

On EL9, run as the cluster owner, or set `POSTVEC_DATABASE_URL`.
`--no-restart` writes valid state and exits **4**. Restart PostgreSQL
before using the cluster.

## Multiple databases

`postvec.database` is cluster-wide. Each `--database` **adds** to the
list.

```bash
sudo postvec setup --database analytics --embedded
```

Switching between remote and embedded requires `--switch-mode` and must
name **every** configured database.

Every name in `postvec.database` must refer to an existing database. A
missing database causes the launcher to repeatedly respawn the failing
worker. `setup` creates the database before activating the list.

## Configuration ownership checks

| State of `99-postvec.conf` | Result |
|---|---|
| Missing / CLI-owned and unchanged | Writable |
| Hand-written (`Foreign`) | Refuse |
| CLI-owned but edited (`Modified`) | Refuse |

`Foreign` and `Modified` files are refused, including with `--yes`.
Reconcile or move the file.

A manually maintained `postvec.conf` should not coexist with the
CLI-owned `99-postvec.conf`.

:::: info Optional
[Verify artifacts](/docs/install/verify) has `doctor --deep` and the SQL
checks (`SHOW postvec.mode`, `postvec.models`, `status()`).
::::

## Manual configuration

Manual configuration keeps the file outside CLI ownership:

```ini
shared_preload_libraries = 'postvec'
postvec.database = 'app'
postvec.mode = 'embedded'
postvec.path = '/opt/postvec'
```

Remote deployments set `postvec.mode = 'grpc'` plus
`postvec.grpc_endpoints` and `postvec.http_endpoints`.

Create the database and `CREATE EXTENSION postvec CASCADE` **before**
adding the name to `postvec.database`, then restart.

`postvec doctor` is read-only. [Verify artifacts](/docs/install/verify)
and the [CLI reference](/docs/reference/cli) cover flags.
