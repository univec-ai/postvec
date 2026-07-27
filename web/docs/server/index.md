---
title: Remote inference
description: postvec-server ports, disk layout and the remote-mode path.
---

# Remote inference

Default mode (`embedded`) runs the engine inside the PostgreSQL launcher.
Remote mode (`grpc`) runs it in `postvec-server`. PostgreSQL talks to that
process over gRPC. SQL, the job queue and the wire contract are the same.
Switching is `postvec.mode` plus a restart.

Use remote mode for crash isolation (an ONNX fault stays in the node
process), a dedicated CPU or memory budget, GPU, sharing one engine
across databases, or a replica that should search.

One process: loads models from a directory, serves three ports, optionally
joins a gossip group. Queue, scheduler and SQL stay in PostgreSQL.

## Ports

| Port | Purpose | Exposure |
|---|---|---|
| `33333` | gRPC inference | Plaintext, unauthenticated. Private networks only |
| `22222` | Discovery (`GET /config`), `/health`, `/ready`, `/metrics`, HTTP inference, [dashboard](/docs/server/dashboard) | TLS by default |
| `22223` | Admin: load, unload, reload providers, registry mutations | Loopback only, enforced at boot. `--manage` also serves the mutations on `22222`. |
| `11111` | Gossip membership | Between nodes |

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

- [Run a node](/docs/server/node)
- [Dashboard](/docs/server/dashboard)
- [Connect PostgreSQL](/docs/server/connect)
- [Models on a node](/docs/server/models)
- [Run a fleet](/docs/server/fleet)
- [HTTP API](/docs/server/http-api)
- [Node reference](/docs/server/reference)

## License

The extension, CLI and their packages are under the PostgreSQL License.
`postvec-server` is **Business Source License 1.1** (source-available;
production use by an organization needs a commercial license from Univec).

- [Embedded vs remote](/docs/concepts/modes)
- [External providers](/docs/models/providers)
- [GUCs](/docs/reference/gucs)
