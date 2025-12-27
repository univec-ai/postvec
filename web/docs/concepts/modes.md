---
title: Embedded vs remote
description: Operational differences between embedded inference and remote gRPC inference.
---

# Embedded vs remote

This site leads with embedded mode: inference on the database host. The
extension, CLI and packages are under the PostgreSQL License. Remote
mode uses a separately operated ninference fleet (community and
organisation licenses) for GPU or distributed inference and for
organisation deployments that serve the private catalogue from that
fleet.

`postvec.mode` is cluster-wide and POSTMASTER. Changing it requires a
restart. SQL, the job queue, retry policy and the gRPC wire contract
stay the same.

| | Embedded | Remote (`grpc`) |
|---|---|---|
| Inference | One engine inside the PostgreSQL launcher | Separately operated ninference nodes |
| Discovery | Loopback HTTP on the launcher | `GET /config` on those nodes |
| DB-host assets | Extension, CLI, ONNX Runtime, models | Extension + CLI |
| Raw text leaves the DB host | Stays on the host | Goes only to the configured ninference service |
| Model commands | `postvec model pull / upgrade / rm / activate` | Local mutation unsupported; models are administered on the fleet |
| Engine crash | Restarts the launcher | Stays outside PostgreSQL |
| Typical use | Single-node, private, edge, air-gapped, regulated | Distributed or GPU workloads; organisation ninference |

The public extension package includes both. `postvec setup --embedded`
selects embedded mode. `setup` with `--grpc` / `--http` selects remote
mode.

## Embedded

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference

postvec model ls
sudo postvec doctor --database app --deep
```

The engine root must contain:

```
{root}/libs/**/libonnxruntime.so     # unversioned filename required
{root}/models/{backend}/{name}/ninference.hub.json
```

Workers keep jobs pending until the engine listener is ready. Models
pulled with `postvec model pull` hot-load.

The embedded gRPC/HTTP listeners stay on **127.0.0.1**.

Inference, weights and text remain on the database host.

## Remote gRPC

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222
```

ninference is a separate CPU or GPU inference server on the deployment
network. Remote mode provides:

- distributed embedding and conversion
- inference memory and compute outside PostgreSQL
- the **private** catalogue served from that fleet
- organisation support around that fleet

Provider credentials, when required, stay on the ninference side. They
are not stored in PostgreSQL. `--allow-unreachable` is only for staging
configuration before the fleet exists.

If the remote engine is unavailable, jobs remain **pending** without
consuming retry attempts. Lexical search can still run while
`postvec.search_degrade_to_fts` is on (the default).

Accounts and support: [univec.ai](https://univec.ai).

## Switching

Mode is cluster-wide. Switching requires every configured database name
and the `--switch-mode` flag:

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference \
  --switch-mode
```

Engine files on disk are unused in remote mode. They are not deleted.

## Selection

:::: warning Embedded mode shares resources with PostgreSQL
Embedded faults restart the launcher. Remote keeps engine faults off the
database host. Embedded mode suits deployments where text must remain on
the database host or no inference fleet exists. Remote mode provides
workload isolation, GPU support and fleet-served private catalogues.
::::
