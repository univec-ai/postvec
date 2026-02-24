---
title: Connect PostgreSQL
description: Point a cluster at postvec-server nodes, prove the pairing with doctor and switch an existing cluster over.
---

# Connect PostgreSQL

A cluster in remote mode needs two endpoint lists: gRPC for inference, HTTP
for discovery. The database host installs the extension and the CLI, and
nothing else. No ONNX Runtime, no models.

## 1. Install on the database host

```bash
sudo apt install ./postvec-cli_*.deb ./postgresql-18-postvec_*.deb
```

The engine-asset packages (`postvec-onnxruntime`,
`postvec-model-minilm-l6-v2`) belong on the node, not here. Full download and
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
postvec.ninference_grpc_endpoints = '10.0.0.20:33333'
postvec.ninference_http_endpoints = 'https://10.0.0.20:22222'
```

With several nodes, give the cluster all of them as comma-separated lists.
postvec round-robins the gRPC list itself:

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333,10.0.0.21:33333,10.0.0.22:33333 \
  --http https://10.0.0.20:22222,https://10.0.0.21:22222,https://10.0.0.22:22222
```

`--allow-unreachable` stages the configuration before a node exists. The
worker starts and inference stays unavailable until one answers.

## 3. Prove the pairing

```bash
sudo postvec doctor --database app --deep
```

:::: tip Expected
Three checks report the connection: `remote.grpc.connect`,
`remote.http.health` and `remote.http.config`. The last one also lists the
models the node advertises, which is what `postvec.models` will hold after the
next refresh.
::::

```sql
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
```

An empty result means discovery has not run yet or the node serves nothing.
`SELECT postvec.refresh_models();` forces a refresh.

If `doctor` reports the node as unreachable, check in this order: the node's
own `/ready`, then the gRPC port through a firewall, then whether the HTTP
endpoint in the GUC matches the scheme the node actually serves. A node
started without `--insecure` serves `https`.

## 4. Use it

Nothing about the SQL changes:

```sql
SELECT postvec.enable('docs', 'body', model => 'baai-bge-m3');
```

The model name must be one the node serves. `postvec.models` is the list.

## Containers

The image reads the same two endpoint lists from the environment:

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

Engine files already on the database host are left alone. They are unused in
remote mode, and deleting them is a separate decision.

Stored vectors are unaffected by the switch. A column keeps its model name and
its dimension, so the node has to serve that model or the column has no
embedding route. List what the cluster needs before switching:

```sql
SELECT DISTINCT model FROM postvec.status();
```

## Related documentation

- [Run a node](/docs/server/node) - the other end of this connection
- [Run a fleet](/docs/server/fleet) - several nodes, and the parity rule
- [Embedded vs remote](/docs/concepts/modes) - the comparison
- [Configure the cluster](/docs/install/setup) - `setup` in general
