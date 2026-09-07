---
title: Installation
description: Docker, packages, managed PostgreSQL or source, then configuration.
---

# Installation

Installation stages:

- Local install - docker image, packages or manual install
- Cluster configuration - `postvec setup`

[postvec-server](/docs/server/) is a separate install: the companion
inference process for remote mode, GPU hosts and managed cloud
databases.

| Host | Path |
|---|---|
| Try it locally | [Quick start local](/docs/quickstart) or [quick start remote](/docs/quickstart-remote) |
| Self-hosted PostgreSQL 16, 17 or 18 | [Packages](/docs/install/packages), then [configure](/docs/install/setup) |
| Inference off the database (GPU, isolation, fleet) | [postvec-server](/docs/server/) ([Docker](/docs/server/docker) or [packages](/docs/server/packages)), then [connect PostgreSQL](/docs/server/connect) |
| RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon | [Managed PostgreSQL](/docs/server/managed) |
| Development tree | [From source](/docs/install/source) |

Self-hosted needs pgvector >= 0.8, superuser and a host that can set
`shared_preload_libraries = 'postvec'` and restart. Without a restart,
`SELECT postvec.start_worker()` runs the worker until the next server
restart.
