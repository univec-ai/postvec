---
title: Choose an installation method
description: Docker, package and source installation for PostgreSQL 16, 17 and 18.
---

# Choose an installation method

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

## Select a method

| Use case | Method |
|---|---|
| Container | [Docker](/docs/install/docker) |
| Debian / Ubuntu / EL9 | [Packages](/docs/install/packages) |
| Manual | [Source](/docs/install/source) |

## Requirements

- PostgreSQL **16, 17 or 18**, matching the artifact.
- **pgvector >= 0.8** for that major.
- Superuser for `CREATE EXTENSION postvec` (untrusted).
- Permission to **restart** PostgreSQL.
- One background-worker slot for the launcher, plus one per configured
  database.
- A host that can set `shared_preload_libraries = 'postvec'`.

## Confirm the files, then configure

```bash
postvec --version
```

Expected: the installed CLI version, for example `postvec 0.1.0`.

Then [configure the cluster](/docs/install/setup). After setup:

```bash
sudo postvec doctor --database app --deep
```

Exit 0 means library and SQL versions match, the heartbeat advances and
the on-disk, engine and SQL inventories agree.

Later: [Upgrade](/docs/install/upgrade) / [Uninstall](/docs/install/uninstall).
