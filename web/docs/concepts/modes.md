---
title: Embedded vs remote
description: Operational differences between embedded inference and remote gRPC inference.
---

# Embedded vs remote

`postvec.mode` is cluster-wide and POSTMASTER: changing it requires a restart.
Both modes share SQL, the job queue, retry policy, and the gRPC wire contract.

**Embedded mode** runs inference on the database host. There is no outbound
embedding API and no account. **Remote mode** uses a separately operated
ninference fleet — the shape for GPU or distributed inference, and for
organisation deployments that serve the private catalogue from that fleet.

| | Embedded | Remote (`grpc`) |
|---|---|---|
| Inference | One engine inside the PostgreSQL launcher | Separately operated ninference nodes |
| Discovery | Loopback HTTP on the launcher | `GET /config` on those nodes |
| DB-host assets | Extension, CLI, ONNX Runtime, models | Extension + CLI |
| Raw text leaves the DB host | No | Only to the configured ninference service, not a SaaS embedding API |
| Model commands | `postvec model pull / upgrade / rm / activate` | Local mutation unsupported; models are administered on the fleet |
| Engine crash | Restarts the launcher | Stays outside PostgreSQL |
| Typical use | Single-node, private, edge, air-gapped, regulated | Distributed or GPU workloads; organisation ninference |

The public extension package includes both. Installing `postvec-embedded` does
not change `postvec.mode`. `postvec setup --embedded` does. Invoking `setup`
with `--grpc` / `--http` and without `--embedded` selects remote mode.

## Embedded

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference

sudo postvec model ls
sudo postvec doctor --database app --deep
```

The engine root must contain:

```
{root}/libs/**/libonnxruntime.so     # unversioned filename required
{root}/models/{backend}/{name}/ninference.hub.json
```

Workers keep jobs pending until the engine listener is ready. Models pulled
with `postvec model pull` hot-load; no PostgreSQL restart.

The embedded gRPC/HTTP listeners are **loopback only**. No environment
variable can move them off `127.0.0.1`.

Inference, weights, and text remain on the database host. Embedded inference
does not use an outbound embedding API.

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

Provider credentials, when required, remain on the ninference side and are
not stored in PostgreSQL. `--allow-unreachable` is only for staging
configuration before the fleet exists.

If the remote engine is unavailable, jobs remain **pending** without consuming
retry attempts. Lexical search can still run while
`postvec.search_degrade_to_fts` is on (the default).

Accounts and support: [univec.ai](https://univec.ai).

## Switching

Mode is cluster-wide. Switching requires every configured database name and
the `--switch-mode` flag:

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference \
  --switch-mode
```

Engine files on disk are unused in remote mode; they are not deleted.

## Selection

:::: warning Embedded mode shares resources with PostgreSQL
Embedded faults restart the launcher. Remote keeps engine faults off the
database host. Embedded mode suits deployments where text must remain on the
database host or no inference fleet exists. Remote mode provides workload
isolation, GPU support, and fleet-served private catalogues.
::::
