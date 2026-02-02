---
title: Choose an installation method
description: Docker, package and source installation for PostgreSQL 16, 17 and 18. Files first, then setup.
---

# Choose an installation method

Installation has two stages:

1. **Deliver files** - an image, packages or a manual copy.
2. **Configure the cluster** - `postvec setup`.

Packages and images place binaries and libraries. `postvec setup` then
writes cluster configuration, creates the extension and starts the
worker. An existing cluster can receive the files and stay up until
that setup step.

**Embedded mode** (`setup --embedded`) runs inference on the host. No
third-party embedding API is involved. Remote mode talks to
`postvec-server` nodes you operate; the postvec repository ships one. See
[embedded vs remote](/docs/concepts/modes).

:::: info Release status
Commands in these guides use the planned `0.1.0-1` artifact identity.
The [release artifacts page](/download) reports whether those packages
and images have been published. Until they are, use locally built
artifacts.
::::

## Select a method

| Use case | Method |
|---|---|
| Test without modifying the host cluster | [Docker](/docs/install/docker) |
| Persistent install on Debian / Ubuntu / EL9 | [Packages](/docs/install/packages) |
| Iterate on extension code | [Source](/docs/install/source) |

Package-owned files and manually copied files must not share a path.
`command -v postvec` should resolve to the intended binary
(`/usr/bin` or `/usr/local/bin`).

Commands that mention a PostgreSQL major have tabs for **16, 17 and
18**. The selected major is remembered across these pages.

## Requirements

- PostgreSQL **16, 17 or 18**, matching the artifact.
- **pgvector >= 0.8** for that major.
- Superuser for `CREATE EXTENSION postvec` (untrusted).
- Permission to **restart** PostgreSQL.
- One background-worker slot for the launcher, plus one per configured
  database.

Not supported: **RDS, Aurora** and any host that forbids
`shared_preload_libraries = 'postvec'`.

## After the files are in place

[Configure the cluster](/docs/install/setup). Later lifecycle:
[Upgrade](/docs/install/upgrade) / [Uninstall](/docs/install/uninstall).
