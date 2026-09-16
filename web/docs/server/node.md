---
title: Install postvec-server
description: Docker, packages or a source build for the companion inference server.
---

# Install postvec-server

A running node serves gRPC on `33333` and TLS discovery on `22222`. It is the
inference host for [remote mode](/docs/server/usage). Embedded mode runs the
engine inside the PostgreSQL process. Point PostgreSQL at the node with
[Connect PostgreSQL](/docs/server/connect), or attach a managed database with
[managed PostgreSQL](/docs/server/managed).

| Method | Choose it when |
|---|---|
| [Docker](/docs/server/docker) | You want the bundled MiniLM model and no host setup. The container generates its own certificate at start |
| [Packages](/docs/server/packages) | You want the systemd unit, the packaged configuration file and upgrades through `apt` or `dnf` |
| [From source](/docs/server/source) | You need a build the packages do not ship, such as a GPU build, or you work on the code |

The image and the packages carry the CPU build. A GPU node is a source build
with the `ort-cuda` or `ort-tensorrt` Cargo feature; see
[From source](/docs/server/source).

The Docker image pairs with the remote PostgreSQL image in
[quick start remote](/docs/quickstart-remote).

::: warning License
`postvec-server` is **Business Source License 1.1** (source-available).
Personal production use, non-production environments and a 30-day
production evaluation per organization are free. Production use by an
organization needs [postvec pro](/server#plans). The extension, CLI,
runtime, model packages and PostgreSQL images stay under the PostgreSQL
License. [License](/docs/license).
:::

- [Connect PostgreSQL](/docs/server/connect)
- [Dashboard](/docs/server/dashboard)
- [Models](/docs/server/models)
- [Reference](/docs/server/reference)
