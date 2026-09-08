---
title: postvec-server
description: Companion inference server for remote mode, GPU fleets, crash isolation and managed PostgreSQL.
---

# postvec-server

postvec-server is the companion inference server for a postvec-enabled
database. The extension stays in PostgreSQL. The server loads models,
answers embed and convert requests, and can run the worker for
[managed PostgreSQL](/docs/server/managed) hosts that cannot load a
native extension.

The default install runs inference inside PostgreSQL (embedded mode).
Point the cluster at postvec-server (`postvec.mode = grpc`) when you
want process isolation from PostgreSQL, multi-threaded inference on the
same VM, a CPU or GPU fleet or model management from the
[dashboard](/docs/server/dashboard) or HTTP API. RDS, Aurora, Cloud SQL,
Azure, Supabase and Neon use it for [managed PostgreSQL](/docs/server/managed).

SQL, the job queue and the wire contract stay the same. Switching is
`postvec setup --grpc` / `--http` plus a restart.
[When to use postvec-server](/docs/server/usage).

One process: loads models from a directory, serves three ports,
optionally joins a gossip group. Queue, scheduler and SQL stay in
PostgreSQL (or, on managed hosts, in the plain SQL schema).

| Situation | Path |
|---|---|
| Try both containers | [Quick start remote](/docs/quickstart-remote) |
| Docker image (default install) | [Docker](/docs/server/docker) |
| `.deb` / `.rpm` | [Packages](/docs/server/packages) |
| Checkout | [From source](/docs/server/source) |
| Point an existing cluster at it | [Connect PostgreSQL](/docs/server/connect) |
| Several processes | [Fleet](/docs/server/fleet) |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | [Managed PostgreSQL](/docs/server/managed) |
| Hosted OpenAI / Cohere / Gemini / ... | Same [provider](/docs/models/providers) files, on each server |

## Ports

| Port | Purpose | Exposure |
|---|---|---|
| `33333` | gRPC inference | Plaintext, unauthenticated. Private networks only |
| `22222` | Discovery (`GET /config`), `/health`, `/ready`, `/metrics`, HTTP inference, [dashboard](/docs/server/dashboard) | TLS by default |
| `22223` | Admin: load, unload, reload providers, registry mutations | Loopback only, enforced at boot. `--manage` also serves the mutations on `22222`. |
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

The model layout is the same one embedded mode uses. A tree that
`postvec model pull` already wrote into works unchanged: name it with
`--root` or `POSTVEC_SERVER_ROOT`.

- [When to use it](/docs/server/usage)
- [Install](/docs/server/node)
  ([Docker](/docs/server/docker), [packages](/docs/server/packages),
  [from source](/docs/server/source))
- [Dashboard](/docs/server/dashboard)
- [Connect PostgreSQL](/docs/server/connect)
- [Models](/docs/server/models)
- [Fleet](/docs/server/fleet)
- [Managed PostgreSQL](/docs/server/managed)
- [HTTP API](/docs/server/http-api)
- [Reference](/docs/server/reference)

## License

The extension, CLI and their packages are under the PostgreSQL License.
`postvec-server` is **Business Source License 1.1**: source-available.
Development, testing, personal use and a 30-day production evaluation are
free. Production use by an organization needs a
[postvec Pro](https://univec.ai) subscription (€30/month, including €30
of UniVec API credit and commercial rights to the private catalogue).
Hosting it for third parties needs an enterprise agreement. Each version
converts to the PostgreSQL License four years after release.

[License](/docs/license).

- [Embedded vs remote](/docs/concepts/modes)
- [External providers](/docs/models/providers)
- [GUCs](/docs/reference/gucs)
