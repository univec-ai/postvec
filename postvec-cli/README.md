# postvec-cli

The `postvec` command: configure and diagnose the [extension](../postvec) on an existing cluster.

Full command reference: [postvec.dev/docs/reference/cli](https://postvec.dev/docs/reference/cli).

```bash
sudo postvec setup --database app --embedded
postvec doctor --database app
sudo postvec uninstall --database app
```

The CLI ships as `postvec-cli`, separate from the per-major `postgresql-NN-postvec` packages, so two PostgreSQL majors can sit side by side with one `/usr/bin/postvec`.

## setup

Creates the database if needed, installs the extension, writes `conf.d/99-postvec.conf`, restarts when a setting changed and checks that the worker heartbeat advances. Idempotent: a rerun with the same arguments plans nothing.

```bash
# Embedded: engine in the PostgreSQL process (default).
sudo postvec setup --database app --embedded --path /opt/postvec

# Remote: postvec-server on the local network.
sudo postvec setup --cluster 18/main --database app \
     --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222

# Second database, same cluster.
sudo postvec setup --database analytics \
     --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222

postvec setup --database app --embedded --dry-run
```

`--model NAME` (repeatable) is an embedded preload allow-list. Omit it to load every enabled model.

## doctor

```bash
postvec doctor
postvec doctor --database app --deep
postvec doctor --format json --strict
```

Read-only. Reports preload, mode, worker heartbeat, model inventory and version match between the library and the catalog.

## uninstall

```bash
sudo postvec uninstall --database app
sudo postvec uninstall --database app \
     --drop-columns --acknowledge-data-loss --yes
```

`DROP EXTENSION` runs without `CASCADE`. Host cleanup (`--purge`) is documented under [uninstall](https://postvec.dev/docs/install/uninstall).

## model

```bash
postvec model ls
postvec model ls --available
sudo postvec model pull baai-bge-m3
sudo postvec model activate baai-bge-m3  # enable on disk and load
sudo postvec model deactivate baai-bge-m3
sudo postvec model upgrade --all
sudo postvec model rm baai-bge-m3
```

`pull` writes `enabled: false`. `activate` / `deactivate` change serving state. [Models](https://postvec.dev/docs/models/).

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | completed; doctor had no FAIL (and no WARN under `--strict`) |
| 1 | apply failure, postcondition failure, or a doctor FAIL |
| 2 | invalid invocation, or a prompt that could not be answered |
| 3 | partial result: a target was left unchanged |
| 4 | changes are in place but the requested restart was deferred |

Host configuration needs root. `sudo postvec setup` keeps root for the filesystem and drops to the cluster owner for database work.
