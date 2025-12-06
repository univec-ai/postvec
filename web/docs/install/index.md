---
title: Choose an install path
description: Docker, packages, or source — and what "installed" actually means.
---

# Choose an install path

Installing postvec is two steps that must stay separate:

1. **Deliver files** — packages, an image, or a manual copy.
2. **Configure a cluster** — `postvec setup`, which you run on purpose.

A package or image never edits `postgresql.conf`, never creates a database,
and never downloads a model. That is why you can install next to existing
clusters without a surprise restart.

This site configures **embedded** first (`setup --embedded`): on-prem
inference, no third-party API. Remote/ninference is the organisation
path — see [embedded vs remote](/docs/concepts/modes).

::: info Release status
The commands in these guides use the planned `0.1.0-1` artifact identity. The
[release artifacts page](/download) reports whether those packages and images
have been published; before that, use locally built artifacts.
:::

## Pick one

| Goal | Path |
|---|---|
| Try it without touching the host cluster | [Docker](/docs/install/docker) |
| Production-shaped install on Debian / Ubuntu / EL9 | [Packages](/docs/install/packages) |
| Iterate on extension code | [Source](/docs/install/source) |

Do not mix package-owned and hand-copied files at the same path.
`command -v postvec` must name the binary you think you are testing
(`/usr/bin` vs `/usr/local/bin`).

## Requirements (every path)

- PostgreSQL **16, 17, or 18**, matching the artifact.
- **pgvector ≥ 0.8** for that major.
- Superuser for `CREATE EXTENSION postvec` (untrusted).
- Permission to **restart** PostgreSQL.
- One background-worker slot for the launcher, plus one per configured
  database.

Not supported: **RDS, Aurora**, and any host that forbids
`shared_preload_libraries = 'postvec'`.

## Then configure

After files are in place: [Configure the cluster](/docs/install/setup).

Later: [Upgrade](/docs/install/upgrade) · [Uninstall](/docs/install/uninstall).
