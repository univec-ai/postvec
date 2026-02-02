---
title: Build from source
description: Source build and manual installation for PostgreSQL 16, 17 or 18.
---

# Build from source

Build from source when iterating on the extension. Persistent
deployments should use [packages](/docs/install/packages). Package-owned
files and manually copied files must not share paths.

Tabs pick the PostgreSQL major and the layout of the target tree.

## Prerequisites

- Rust 1.96 or newer
- cargo-pgrx **0.18.1**
- `protoc`
- PostgreSQL server-development headers for the target major
- pgvector >= 0.8 built against the same `pg_config`

<PgSnippet id="source-prereq" />

## Build

The extension is **not** in the Cargo workspace. The CLI remains a
workspace member.

<PgSnippet id="source-build" />

Staged extension: `postvec/target/release/postvec-pgNN/`.
CLI: `target/release/postvec`. The build stays inside the repository.

## Install into a pgrx cluster

```bash
cd postvec
cargo pgrx install --release \
  --no-default-features \
  --features pg18,embedded \
  --pg-config /path/to/development/postgres/bin/pg_config
```

Replace `pg18` with `pg16` or `pg17` to match that development cluster.

## Install into a system PGDG tree

These files have **no** package owner. Every copied path needs a record
so it can be removed later.

<PgSnippet id="source-install-pgdg" />

## Embedded engine root

A source build has no ONNX Runtime and no weights. Install the
engine-asset packages, reuse an existing engine root or copy the
packaging payloads:

```bash
cd packaging/postvec
scripts/build-onnxruntime-bundle.sh --arch amd64
# The bundled model comes from the postvec model registry's public channel,
# pulled by the postvec CLI built above.
scripts/build-model-bundle.sh

sudo install -d -m 0755 /opt/postvec/ninference
sudo cp -a build/payload-amd64/opt/postvec/ninference/libs \
  /opt/postvec/ninference/
sudo cp -a build/payload-common/opt/postvec/ninference/models \
  /opt/postvec/ninference/
```

Manually copied models are operator-owned. `postvec model rm` will not
delete them.

## Development rebuild sequence

1. `cargo pgrx package`
2. Stop PostgreSQL before replacing a preloaded `.so`
3. Copy the staged files
4. Recreate a disposable database if same-version generated SQL changed

A new `.so` does not change SQL already in a database. A new extension
version needs upgrade SQL plus `ALTER EXTENSION postvec UPDATE` in the
same window as the restart. [Upgrade](/docs/install/upgrade).

## Rollback (files only)

Stop PostgreSQL first. Confirm that each path is **not** package-owned
with `dpkg -S` or `rpm -qf`. Remove only recorded files, then restore
any previous manual installation while the cluster remains stopped.

[Cluster configuration](/docs/install/setup) follows file installation.
If `setup` already ran, finish
[uninstallation](/docs/install/uninstall) before deleting files.
