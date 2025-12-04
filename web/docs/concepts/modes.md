---
title: Embedded vs remote
description: Embedded is the default path on this site. Remote grpc is the organisation product.
---

# Embedded vs remote

`postvec.mode` is cluster-wide and POSTMASTER: changing it requires a restart.
Both modes share SQL, the job queue, retry policy, and the gRPC wire contract.

This site teaches **embedded first**. That is the on-prem, no-account,
no-third-party-API path. Remote mode is the organisation product: a
ninference fleet, the private catalogue, and support.

| | Embedded | Remote (`grpc`) |
|---|---|---|
| Inference | One engine inside the PostgreSQL launcher | ninference node(s) you operate |
| Discovery | Loopback HTTP on the launcher | `GET /config` on those nodes |
| DB-host assets | Extension, CLI, ONNX Runtime, models | Extension + CLI |
| Raw text leaves the DB host | No | To your ninference service only — not a SaaS embed API |
| Model commands | `postvec model pull / upgrade / rm / activate` | Local mutation refused; administer the fleet |
| Engine crash | Restarts the launcher | Stays outside PostgreSQL |
| Who it is for | Single node, private, edge, air-gapped, compliance | Distributed / GPU workloads, org catalogue, support |

The public extension package includes both. Installing
`postvec-embedded` does not flip the mode. `postvec setup --embedded`
does. If you run `setup` with `--grpc` / `--http` and no `--embedded`,
you get remote.

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

Workers keep jobs pending until the engine listener is ready. Models
pulled with `postvec model pull` hot-load; no PostgreSQL restart.

The embedded gRPC/HTTP listeners are **loopback only**. No environment
variable can move them off `127.0.0.1`.

Inference, weights and text stay on the database host. No outbound
embedding API.

## Remote — organisations

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222
```

ninference is a separate inference server (CPU or GPU) that you run on
your network. It is how you:

- spread embedding and conversion across machines
- keep PostgreSQL thin
- serve the **private** catalogue (full embed suite, 100+ conversion
  pairs, higher-fidelity converters)
- get organisation support

Provider credentials, if you use any at all, live on the ninference
side — never in PostgreSQL. `--allow-unreachable` is only for staging
configuration before the fleet exists.

If the remote engine is down, jobs stay **pending** rather than burning
retries. Lexical search can still run while
`postvec.search_degrade_to_fts` is on (the default).

Accounts and support: [univec.ai](https://univec.ai).

## Switching

Mode is cluster-wide. Name every configured database and pass
`--switch-mode`:

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference \
  --switch-mode
```

Engine files on disk are unused in remote mode; they are not deleted.

## Don't

::: danger Don't pick embedded to "go faster" on a busy primary
Embedded faults restart the launcher. Remote keeps engine faults off the
database host. Use embedded when text must not leave, or when there is
no fleet. Use remote when you want isolation, GPUs, or the org catalogue.
:::
