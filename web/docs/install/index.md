---
title: Choose an installation method
description: Docker, package and source installation. Files first, then setup.
---

# Choose an installation method

Putting files on disk and configuring a cluster are two different steps.

1. **Deliver files** - an image, packages or a manual copy.
2. **Configure a cluster** - an explicit `postvec setup` invocation.

Packages and images leave `postgresql.conf` alone. They do not create a
database and they do not download a model. An existing cluster can receive
the files without a restart.

**Embedded mode** (`setup --embedded`) runs inference on the host. No
third-party embedding API is involved. Remote mode talks to ninference
nodes operated separately. See [embedded vs remote](/docs/concepts/modes).

:::: info Release status
Commands in these guides use the planned `0.1.0-1` artifact identity.
The [release artifacts page](/download) reports whether those packages and
images have been published. Until they are, use locally built artifacts.
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
