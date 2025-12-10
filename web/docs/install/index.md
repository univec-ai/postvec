---
title: Installation options
description: Docker, package, and source installation options.
---

# Installation options

File installation and cluster configuration are separate operations:

1. **Deliver files** — packages, an image, or a manual copy.
2. **Configure a cluster** — an explicit `postvec setup` invocation.

A package or image does not edit `postgresql.conf`, create a database, or
download a model. Files can therefore be installed alongside an
existing cluster without causing a restart.

**Embedded mode** (`setup --embedded`) provides on-prem inference with no
third-party embedding API. Remote mode uses separately operated ninference
nodes. See [embedded vs remote](/docs/concepts/modes).

::: info Release status
The commands in these guides use the planned `0.1.0-1` artifact identity. The
[release artifacts page](/download) reports whether those packages and images
have been published; before that, use locally built artifacts.
:::

## Select an installation method

| Use case | Method |
|---|---|
| Test without modifying the host cluster | [Docker](/docs/install/docker) |
| Persistent install on Debian / Ubuntu / EL9 | [Packages](/docs/install/packages) |
| Iterate on extension code | [Source](/docs/install/source) |

Package-owned and manually copied files must not share a path.
`command -v postvec` should resolve to the intended binary
(`/usr/bin` or `/usr/local/bin`).

## Requirements

- PostgreSQL **16, 17, or 18**, matching the artifact.
- **pgvector ≥ 0.8** for that major.
- Superuser for `CREATE EXTENSION postvec` (untrusted).
- Permission to **restart** PostgreSQL.
- One background-worker slot for the launcher, plus one per configured
  database.

Not supported: **RDS, Aurora**, and any host that forbids
`shared_preload_libraries = 'postvec'`.

## Cluster configuration

[Configure the cluster](/docs/install/setup) after installing the required
files.

Lifecycle procedures: [Upgrade](/docs/install/upgrade) ·
[Uninstall](/docs/install/uninstall).
