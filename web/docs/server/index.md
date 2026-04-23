---
title: Remote inference
description: What postvec-server is, when remote mode is worth the extra process and what a node holds on disk.
---

# Remote inference

Embedded mode runs the inference engine inside the PostgreSQL launcher. It
needs nothing else running and it is the default. Remote mode moves that
engine to `postvec-server`, a node you operate, and PostgreSQL talks to it over
gRPC.

Both modes speak the same SQL, the same job queue and the same wire contract.
Switching is a setting and a restart, not a different build or a rewrite.
`postvec-server` ships in the postvec repository.

## When it is worth a second process

Move inference out when one of these is true:

- **Crash domain.** A native fault in ONNX Runtime takes down the process it
  runs in. In embedded mode that process is PostgreSQL's launcher.
- **CPU or memory.** The engine competes with the database for both. Embedded
  mode runs under a deliberately conservative host policy for that reason. A
  dedicated node does not have to.
- **GPU.** Embedded mode is CPU-only.
- **Sharing.** Several databases, or several services, against one engine.
- **Read replicas.** A replica that should search but not embed.

Traffic volume alone is not on that list. A two-person team with 40k rows may
want remote mode for the crash domain, and that is an ordinary way to run this.

## What a node is

One process. It loads models from a directory, serves three ports and joins an
optional gossip group so that the nodes can report on each other. It has no
database, no queue and no scheduler. All of that stays in PostgreSQL.

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
for `postvec-server` are not settled yet and are not stated here. The omission
is deliberate rather than an oversight, and it will be stated before the first
release.

## Related documentation

- [Embedded vs remote](/docs/concepts/modes) - the comparison, and switching
- [External providers](/docs/models/providers) - hosted models on a node
- [GUCs](/docs/reference/gucs) - the cluster-side settings remote mode uses
