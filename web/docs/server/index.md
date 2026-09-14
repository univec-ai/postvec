---
title: postvec-server
description: Companion inference server for remote mode, GPU fleets, process isolation and managed PostgreSQL.
---

# postvec-server

postvec-server is the companion inference server for a postvec-enabled database.
The extension stays in PostgreSQL; the server loads the models, answers embed and
convert requests, and administers them from a dashboard or an HTTP API.

It is the second inference host. The default install runs inference inside the
PostgreSQL process (embedded mode). Point the cluster at a server
(`postvec.mode = grpc`) when you need one of these:

- **A separate process.** Inference runs in its own process, so a native ONNX
  fault stops the node and leaves the PostgreSQL launcher running.
- **More threads on one VM.** The embedded engine is a single thread inside the
  database process. postvec-server runs multi-threaded alongside PostgreSQL.
- **A fleet.** Models run on CPU or GPU hosts on the network; every node loads
  the same set and postvec spreads requests across the node list. A GPU node
  needs a build with the `ort-cuda` or `ort-tensorrt` Cargo feature. The
  published image and packages carry the CPU build.
- **Remote model management.** Pull, activate, load and inspect models from the
  [dashboard](/docs/server/dashboard) or the [HTTP API](/docs/server/http-api).
- **Managed PostgreSQL.** RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase
  and Neon cannot load `postvec.so`. There, postvec-server installs a plain SQL
  schema, runs the worker and can proxy `search(text)`.
  [Managed PostgreSQL](/docs/server/managed).

SQL, the job queue and the sync behaviour are identical in both modes. Switching
an existing cluster is `postvec setup --grpc ... --http ... --switch-mode`,
naming every configured database, plus a restart.
[Embedded vs remote](/docs/concepts/modes).

## What runs where

One server process loads models from a directory, serves its ports and can join a
gossip group. The queue, the scheduler and the SQL surface stay in PostgreSQL, or
in the plain SQL schema on a managed host.

| Situation | Path |
|---|---|
| Try both containers | [Quick start remote](/docs/quickstart-remote) |
| Docker image | [Docker](/docs/server/docker) |
| `.deb` / `.rpm` | [Packages](/docs/server/packages) |
| Checkout | [From source](/docs/server/source) |
| Point an existing cluster at it | [Connect PostgreSQL](/docs/server/connect) |
| Several processes | [Fleet](/docs/server/fleet) |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | [Managed PostgreSQL](/docs/server/managed) |
| Hosted OpenAI, Cohere, Gemini or others | The same [provider](/docs/models/providers) files, on each server |

## Ports

| Port | Purpose | Exposure |
|---|---|---|
| `33333` | gRPC inference | Plaintext and unauthenticated. Private networks only |
| `22222` | Discovery (`GET /config`), `/health`, `/ready`, `/metrics`, HTTP inference, [dashboard](/docs/server/dashboard) | TLS by default |
| `22223` | Admin: load, unload, reload providers, registry mutations, the dashboard and the `/admin/managed*` routes | Loopback only, enforced at boot. `--manage` also serves the mutations on `22222` |
| `11111` | Gossip membership | Between nodes |
| `5433` (typical) | Optional pgwire proxy for managed `search(text)` | Same network as the application |

## Disk contents

```text
$root/
├── libs/**/libonnxruntime.so     ONNX Runtime (the postvec-onnxruntime package)
├── models/<backend>/<name>/      descriptor and weights
├── providers.d/*.toml            external providers, optional (0700 dir, 0600 files)
├── server/ui/                    dashboard (package and image)
└── certs/server.{crt,key}        TLS for the discovery listener, optional path
```

The model layout is the one embedded mode uses, so a tree that
`postvec model pull` already wrote works unchanged: point `--root` or
`POSTVEC_SERVER_ROOT` at it.

## License

`postvec-server` is source-available under the Business Source License 1.1
(`BUSL-1.1`) and each version becomes PostgreSQL-licensed four years after
release. Development, testing, personal production use and one 30-day production
evaluation per organization are free. Production use by an organization needs a
[postvec Pro](https://univec.ai) subscription at €30/month, which includes €30 of
UniVec API credit and commercial rights to the private catalogue. Hosting the
server for third parties needs an enterprise agreement.

The extension, the CLI and their packages use the PostgreSQL License in either
inference mode. [License](/docs/license).

- [When to use it](/docs/server/usage)
- [Install](/docs/server/node) ([Docker](/docs/server/docker),
  [packages](/docs/server/packages), [from source](/docs/server/source))
- [Connect PostgreSQL](/docs/server/connect)
- [Dashboard](/docs/server/dashboard) - [Models](/docs/server/models)
- [Fleet](/docs/server/fleet) - [Managed PostgreSQL](/docs/server/managed)
- [HTTP API](/docs/server/http-api) - [Reference](/docs/server/reference)
- [External providers](/docs/models/providers) - [GUCs](/docs/reference/gucs)
