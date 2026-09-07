---
title: Connect PostgreSQL
description: Point a cluster at postvec-server nodes, prove the pairing with doctor and switch an existing cluster over.
---

# Connect PostgreSQL

A cluster in remote mode talks to [postvec-server](/docs/server/). It
needs two endpoint lists: gRPC for inference, HTTP for discovery. The
database host installs the extension and the CLI. Engine assets go on
postvec-server.

## 1. Install on the database host

```bash
sudo apt install ./postvec-cli_*.deb ./postgresql-18-postvec_*.deb
```

The engine-asset packages (`postvec-onnxruntime`,
`postvec-model-minilm-l6-v2`) go on the node. Full download and
verification steps: [packages](/docs/install/packages).

## 2. Point the cluster at the node

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222
```

That writes `conf.d/99-postvec.conf`, merges `shared_preload_libraries`,
creates the extension and restarts. The three settings it records are:

```text
postvec.mode = grpc
postvec.grpc_endpoints = '10.0.0.20:33333'
postvec.http_endpoints = 'https://10.0.0.20:22222'
```

With several nodes, give the cluster all of them as comma-separated lists.
postvec round-robins the gRPC list itself:

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333,10.0.0.21:33333,10.0.0.22:33333 \
  --http https://10.0.0.20:22222,https://10.0.0.21:22222,https://10.0.0.22:22222
```

`--allow-unreachable` stages the configuration before a node exists. The
worker starts. Inference waits until a node answers.

## 3. Models

```sql
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
```

An empty result means discovery has not run yet or the node serves nothing.
`SELECT postvec.refresh_models();` forces a refresh.

:::: info Optional
```bash
sudo postvec doctor --database app --deep
```

`remote.grpc.connect`, `remote.http.health` and `remote.http.config`
report the pairing. If those fail: the node's `/ready`, the gRPC port
through a firewall, then whether the HTTP endpoint matches the scheme
the node serves (`https` unless `--insecure`).
[Verify artifacts](/docs/install/verify).
::::

## 4. SQL

SQL is unchanged:

```sql
SELECT postvec.enable('docs', 'body', model => 'baai-bge-m3');
```

The model name must be one the node serves. `postvec.models` is the list.

## Containers

The image reads the same two endpoint lists from the environment.
[Quick start remote](/docs/quickstart-remote) starts both containers on
one network, already pointed at each other.

<PgSnippet id="docker-remote" />

The base tag pins `POSTVEC_MODE=grpc` and carries no engine assets, so it is
the right image for remote mode. [Docker](/docs/install/docker) has the volume
paths and the full environment table.

## Switching an existing cluster

Mode is cluster-wide and POSTMASTER, so switching needs a restart, the
`--switch-mode` flag and **every** configured database named in one command:

```bash
sudo postvec setup --database app --database analytics \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222 \
  --switch-mode
```

Engine files already on the database host are left alone. Deleting them is a
separate decision.

Stored vectors are unaffected by the switch. A column keeps its model name and
its dimension, so the node has to serve that model or the column has no
embedding route. List what the cluster needs before switching:

```sql
SELECT DISTINCT model FROM postvec.status();
```

- [Install](/docs/server/node)
- [Dashboard](/docs/server/dashboard)
- [Fleet](/docs/server/fleet)
- [Embedded vs remote](/docs/concepts/modes)
- [Configure the cluster](/docs/install/setup)
