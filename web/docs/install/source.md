---
title: Build from source
description: cargo-pgrx package and a manual copy into a development PostgreSQL.
---

# Build from source

Use this to iterate on the extension. For a host you intend to keep, prefer
[packages](/docs/install/packages). Do not let apt and a manual copy compete
for the same paths.

## Prerequisites (you install these)

- Rust 1.96 or newer
- cargo-pgrx **0.18.1**
- `protoc`
- PostgreSQL server-development headers for the target major
- pgvector ≥ 0.8 built against the same `pg_config`

```bash
rustc --version
cargo pgrx --version
protoc --version
/usr/lib/postgresql/18/bin/pg_config --version
```

## Build

The extension is **not** in the Cargo workspace. The CLI is.

```bash
cd postvec
cargo pgrx package \
  --no-default-features \
  --features pg18,embedded \
  --pg-config /usr/lib/postgresql/18/bin/pg_config

cd ..
cargo build --release -p postvec-cli
```

Staged extension: `postvec/target/release/postvec-pg18/`.
CLI: `target/release/postvec`. Nothing outside the repo has changed yet.

## Install into a pgrx cluster

```bash
cd postvec
cargo pgrx install --release \
  --no-default-features \
  --features pg18,embedded \
  --pg-config /path/to/development/postgres/bin/pg_config
```

## Install into a system PGDG tree

These files have **no** package owner. Record every path you copy.

```bash
export PV_STAGE=postvec/target/release/postvec-pg18

sudo install -m 0755 \
  "$PV_STAGE/usr/lib/postgresql/18/lib/postvec.so" \
  /usr/lib/postgresql/18/lib/postvec.so
sudo install -m 0644 \
  "$PV_STAGE/usr/share/postgresql/18/extension/postvec.control" \
  "$PV_STAGE/usr/share/postgresql/18/extension/postvec--0.1.0.sql" \
  /usr/share/postgresql/18/extension/

sudo install -m 0755 target/release/postvec /usr/local/bin/postvec
```

## Embedded engine root

A source build has no ONNX Runtime and no weights. Either install the
engine-asset packages, reuse an existing ninference root, or copy the
packaging payloads:

```bash
cd packaging/postvec
scripts/build-onnxruntime-bundle.sh --arch amd64
scripts/build-model-bundle.sh

sudo install -d -m 0755 /opt/postvec/ninference
sudo cp -a build/payload-amd64/opt/postvec/ninference/libs \
  /opt/postvec/ninference/
sudo cp -a build/payload-common/opt/postvec/ninference/models \
  /opt/postvec/ninference/
```

Manually copied models are operator-owned. `postvec model rm` will not
delete them.

## Rebuild loop

1. `cargo pgrx package`
2. Stop PostgreSQL before replacing a preloaded `.so`
3. Copy the staged files
4. Recreate a disposable database if same-version generated SQL changed

Installing a new `.so` does not change SQL already in a database. A new
extension version needs upgrade SQL plus `ALTER EXTENSION postvec UPDATE`
in the same window as the restart. [Upgrade](/docs/install/upgrade).

## Rollback (files only)

Stop PostgreSQL. Prove the paths are **not** package-owned (`dpkg -S` /
`rpm -qf`). Remove only the files you recorded. Restore a backup of a
previous hand install while the cluster is still stopped.

Then [configure](/docs/install/setup) — or, if you already ran `setup`,
[uninstall](/docs/install/uninstall) before deleting files.
