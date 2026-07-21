---
title: Installation
description: Docker, package and source installation for PostgreSQL 16, 17 and 18.
---

# Installation

Installation stages:

1. **Local install** - Docker image, packages or manual install.
2. **Cluster configuration** - `postvec setup`.

**Embedded mode** (`setup --embedded`) runs inference on the host.
**Remote mode** talks to `postvec-server` nodes. See
[embedded vs remote](/docs/concepts/modes) and
[remote inference](/docs/server/).

:::: info Release status
Commands in these guides use the planned `0.1.0-1` artifact identity.
The [release artifacts page](/download) reports whether those packages
and images have been published. Until they are, use locally built
artifacts.
::::

- [Docker](/docs/install/docker)
- [Packages](/docs/install/packages) (Debian, Ubuntu, EL9)
- [Build from source](/docs/install/source)
- [Verify artifacts](/docs/install/verify) (checksums, attestations, `doctor`)

## Requirements

- PostgreSQL **16, 17 or 18**, matching the artifact.
- **pgvector >= 0.8** for that major.
- Superuser for `CREATE EXTENSION postvec` (untrusted).
- Permission to **restart** PostgreSQL.
- One background-worker slot for the launcher, plus one per configured
  database.
- A host that can set `shared_preload_libraries = 'postvec'`.

## After install

[Configure the cluster](/docs/install/setup). `setup` already checks that
the worker heartbeat advances.

:::: info Optional
[Verify artifacts](/docs/install/verify) has checksums, Sigstore
attestations, file checks and `postvec doctor --deep`.
::::

[Upgrade](/docs/install/upgrade). [Uninstall](/docs/install/uninstall).
