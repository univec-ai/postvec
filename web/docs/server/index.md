---
title: Remote inference
description: What postvec-server is, when remote mode is worth the extra process and what a node holds on disk.
---

# Remote inference

Default mode (`embedded`) runs the engine inside the PostgreSQL launcher.
Remote mode (`grpc`) runs it in `postvec-server`. PostgreSQL talks to that
process over gRPC. SQL, the job queue and the wire contract are the same.
Switching is `postvec.mode` plus a restart.

`postvec-server` ships in the postvec repository.

## When to run remote mode

- **Crash domain.** An ONNX fault takes down the process it runs in. In
  embedded mode that is PostgreSQL's launcher.
- **CPU or memory.** Embedded mode uses a conservative host policy so the
  engine does not starve the database.
- **GPU.** Embedded mode is CPU-only.
- **Sharing.** Several databases or services against one engine.
- **Read replicas.** A replica that should search but not embed.

## What a node is

One process: loads models from a directory, serves three ports, optionally
joins a gossip group. Queue, scheduler and SQL stay in PostgreSQL.

| Port | Purpose | Exposure |
|---|---|---|
| `33333` | gRPC inference | Plaintext, unauthenticated. Private networks only |
| `22222` | `GET /config` discovery, `/health`, `/ready`, `/metrics` | TLS by default |
| `22223` | Admin: load, unload, reload providers | Loopback only, enforced at boot |
| `11111` | Gossip membership | Between nodes |

## What it holds on disk

```text
$root/
├── libs/**/libonnxruntime.so     ONNX Runtime (the postvec-onnxruntime package)
├── models/<backend>/<name>/      descriptor and weights
├── providers.d/*.toml            external providers, optional (0700 dir, 0600 files)
└── certs/server.{crt,key}        TLS for the discovery listener, optional path
```

The layout is the same one embedded mode uses. A model root is portable
between an in-database engine and a node, and a tree that
`postvec model pull` already wrote into works unchanged: name it with
`--root` or `POSTVEC_SERVER_ROOT`.

## The path

1. [Run a node](/docs/server/node) - files, TLS and the first start.
2. [Connect PostgreSQL](/docs/server/connect) - point a cluster at it and
   prove the pairing.
3. [Models on a node](/docs/server/models) - pull, activate, load, remove.
4. [Run a fleet](/docs/server/fleet) - several nodes, and the parity rule that
   makes them interchangeable.
5. [Node reference](/docs/server/reference) - every flag, the health routes,
   metrics and troubleshooting.

## Licensing

The extension, CLI and their packages are under the PostgreSQL License. Terms
for `postvec-server` will be stated before the first release.

## Related documentation

- [Embedded vs remote](/docs/concepts/modes) - the comparison, and switching
- [External providers](/docs/models/providers) - hosted models on a node
- [GUCs](/docs/reference/gucs) - the cluster-side settings remote mode uses
