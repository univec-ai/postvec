---
title: Installation
description: Docker, packages, managed PostgreSQL or source, then configuration.
---

# Installation

Installation stages:

- Local install - docker image, packages or manual install
- Cluster configuration - `postvec setup`

Managed cloud databases use [postvec-server](/docs/server/managed) with
a plain SQL schema.

| Host | Path |
|---|---|
| Try it locally | [Docker](/docs/install/docker) or [quick start](/docs/quickstart) |
| Self-hosted PostgreSQL 16, 17 or 18 | [Packages](/docs/install/packages), then [configure](/docs/install/setup) |
| RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon | [Managed PostgreSQL](/docs/server/managed) |
| Development tree | [From source](/docs/install/source) |

Self-hosted needs pgvector >= 0.8, superuser and a host that can set
`shared_preload_libraries = 'postvec'` and restart. Without a restart,
`SELECT postvec.start_worker()` runs the worker until the next server
restart.
