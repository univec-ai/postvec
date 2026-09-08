---
title: Install postvec-server
description: Docker, packages or a source build for the companion inference server.
---

# Install postvec-server

After install you have a process serving gRPC on `33333` and discovery
on `22222`, with at least one model loaded. Point PostgreSQL at it with
[Connect PostgreSQL](/docs/server/connect), or attach a managed database
with [managed PostgreSQL](/docs/server/managed).

Both containers together:
[quick start remote](/docs/quickstart-remote).

| Method | When |
|---|---|
| [Docker](/docs/server/docker) | Image with MiniLM already loaded |
| [Packages](/docs/server/packages) | `.deb` / `.rpm` and a systemd unit |
| [From source](/docs/server/source) | A checkout, no PostgreSQL headers |

::: warning License
`postvec-server` is **Business Source License 1.1** (source-available).
Personal production use, non-production environments and a 30-day
production evaluation per organization are free. Production use by an
organization needs [postvec Pro](https://univec.ai). The extension, CLI,
runtime, model packages and PostgreSQL images stay under the PostgreSQL
License. [License](/docs/license).
:::

- [Connect PostgreSQL](/docs/server/connect)
- [Dashboard](/docs/server/dashboard)
- [Models](/docs/server/models)
- [Reference](/docs/server/reference)
