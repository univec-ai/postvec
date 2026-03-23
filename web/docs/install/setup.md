---
title: Configure the cluster
description: Cluster configuration and diagnostics with postvec setup and doctor, for PostgreSQL 16, 17 and 18.
---

# Configure the cluster

`postvec setup` is the step that actually turns files into a working
extension. After packages or a source copy, run this.

It creates missing databases, installs the extension, merges
`shared_preload_libraries`, writes **one** owned file
(`conf.d/99-postvec.conf`), validates it, restarts or reloads if needed,
refreshes models and verifies that the worker heartbeat **advances**.

A dry run reports the planned changes without applying them:

```bash
sudo postvec setup --database app ... --dry-run
```

## Embedded

The engine runs in the launcher. No account or outbound embedding API is
required. This is the extension's **default** mode, and
`postvec.ninference_path` defaults to `/opt/postvec/ninference`, where
the engine-asset packages install, so `setup --embedded` needs no
`--path` on a package install.

`setup` still has to run: it is what enrols the database in
`postvec.database`, installs the extension and restarts. The defaults
remove the *inference* configuration, not the enrolment.

<PgSnippet id="setup-embedded" />

:::: tip Expected
`model ls` shows MiniLM as loaded. `doctor` reports aligned library/SQL
versions, an advancing heartbeat and matching on-disk / engine / SQL
inventories.
::::

`--model NAME` (repeatable) is an embedded preload allow-list. Omit it
to scan-load every enabled descriptor.

`--providers-path DIR` moves the [external provider](/docs/models/providers)
connector directory. Omit it to keep `/etc/postvec/providers.d`. `setup
--embedded` creates that directory empty (`0700`, cluster owner) if it is
absent. An empty or missing directory changes nothing.

## Remote gRPC

Remote mode uses `postvec-server` nodes on the local network. Pick it
for GPU inference, for keeping engine faults off the database host or
for one engine shared by several databases. The SQL surface is
unchanged. See [embedded vs remote](/docs/concepts/modes), and
[remote inference](/docs/server/) for the node side.

Every node must carry the same enabled models: postvec round-robins the
configured endpoints, so a converter present on two nodes out of three
fails one request in three.

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222

sudo postvec doctor --database app --deep
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
`--no-restart` writes valid state and exits **4**. PostgreSQL must then
be restarted before running `doctor`.

## Multiple databases

`postvec.database` is cluster-wide. Each `--database` **adds**; it does
not remove the others.

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

`--yes` does **not** override those two. Reconcile or move the file.

A manually maintained `postvec.conf` should not coexist with the
CLI-owned `99-postvec.conf`.

## SQL and CLI verification

:::: code-group

```sql [SQL]
SELECT postvec.version(), postvec.build_info();
SELECT extname, extversion FROM pg_extension
 WHERE extname IN ('vector', 'postvec');
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
SELECT * FROM postvec.status();
```

```bash [CLI]
sudo postvec doctor --database app --deep
sudo postvec doctor --database app --deep --format json \
  | jq '{summary, failures: [.checks[] | select(.status == "FAIL")]}'
```

::::

:::: tip Expected
Both extensions are present. At least one model is listed when inference
is reachable. `worker_last_beat` is recent and advances between samples.
`doctor` exit 0 is clean.
::::

## Manual configuration

Manual configuration keeps the file outside CLI ownership:

```ini
shared_preload_libraries = 'postvec'
postvec.database = 'app'
postvec.mode = 'embedded'
postvec.ninference_path = '/opt/postvec/ninference'
```

Remote deployments set `postvec.mode = 'grpc'` and the two endpoint
GUCs. `setup` is preferred over manual editing.

Create the database and `CREATE EXTENSION postvec CASCADE` **before**
adding the name to `postvec.database`, then restart.

## `doctor` summary

`doctor` is read-only and performs approximately 40 checks, each with a
remediation. `--deep` verifies that the heartbeat advanced and hashes
CLI-installed model receipts. `--strict` fails on warnings.
`--format json` is the automation surface. Exit 0 is clean.

Full flag list: [CLI reference](/docs/reference/cli).
