---
title: Run a node
description: Install the files, choose TLS or --insecure, start postvec-server and read its boot log.
---

# Run a node

One node, from an empty directory. The result is a process serving gRPC on
`33333` and discovery on `22222`, with at least one model loaded.

## 1. Get the binary

There is no `.deb` or `.rpm` for `postvec-server` yet, and no published image.
Today it comes from a checkout:

```bash
cargo build --release -p postvec-server
sudo install -m 0755 target/release/postvec-server /usr/local/bin/
```

It is an ordinary workspace member, so it needs neither pgrx nor PostgreSQL
headers. The [release page](/download) is the source for publication status.

## 2. Put the files in place

A node needs ONNX Runtime and at least one model under the same root:

```bash
sudo mkdir -p /var/lib/postvec-server
```

Both come from the same packages the database host uses:

| Artifact | Installs |
|---|---|
| `postvec-onnxruntime` | `libonnxruntime.so` under `/opt/postvec/libs` |
| `postvec-model-minilm-l6-v2` | The bundled 384-d model |
| `postvec-cli` | `/usr/bin/postvec`, for `model` and `provider` commands |

Point `--root` at `/opt/postvec` to use those directly, or copy the
tree to `/var/lib/postvec-server` and keep the node's inventory separate from
any local cluster. See [packages](/docs/install/packages) for the download and
verification steps, and [models on a node](/docs/server/models) for pulling
more.

## 3. Decide about TLS

The discovery listener requires TLS. Without `--insecure` and without a
readable certificate pair, the node **refuses to start**.

That refusal is deliberate. Operators write
`POSTVEC_HTTP_ENDPOINTS=https://...` into a cluster, and a silent fallback to
plain HTTP produces a listener that answers and a client that fails
confusingly.

Self-signed certificates are accepted. postvec's discovery client tolerates
them, because the trust boundary here is the network rather than the PKI.

```bash
# Local development, no certificates.
postvec-server --root /var/lib/postvec-server --insecure

# A real node.
postvec-server \
  --root /var/lib/postvec-server \
  --ssl-cert /etc/postvec-server/tls.crt \
  --ssl-cert-key /etc/postvec-server/tls.key
```

A pair dropped at `$root/certs/server.crt` and `$root/certs/server.key` is
found without flags.

## 4. Start it

### As a container

A checkout builds an image carrying the binary, ONNX Runtime and the bundled
model, which are the same payloads the complete postvec image runs in-process:

```bash
cd packaging/postvec
scripts/build-onnxruntime-bundle.sh --arch amd64
scripts/build-model-bundle.sh
scripts/build-server-image.sh --arch amd64

docker run -d --name postvec-server \
  -p 22222:22222 -p 33333:33333 \
  postvec-server:amd64
```

The container generates its own self-signed certificate at start, per
container, never baked into the image. Mount a pair over `/opt/postvec/certs`
or pass `--ssl-cert` / `--ssl-cert-key` to override it. The healthcheck is
`/ready`, so `docker inspect` reports healthy only once a model can answer.

No server image is published yet. The [release page](/download) is the source
for publication status.

### As a service

The repository ships a systemd unit under `postvec-server/systemd/`. It runs
the process unprivileged, with a strict sandbox and a read-only model root.

## 5. Read the boot log

```text
postvec-server 0.1.0 (onnx)
engine root: /var/lib/postvec-server
configuration file: none found; using flags, environment and defaults
advertising 10.0.0.10
gRPC address: 10.0.0.10:33333
HTTP address: https://10.0.0.10:22222
1 model root(s) resolve to 1 resident model(s) (ceiling 16)
loaded model "sentence-transformers-all-minilm-l6-v2"
warming up 1 model(s)
gRPC listening on 0.0.0.0:33333 (plaintext, unauthenticated - private networks only)
discovery listening on https://0.0.0.0:22222
admin listening on http://127.0.0.1:22223 (loopback only)
serving: 1 model(s) ready, ...
```

Two lines repay reading every time:

- **`advertising ...`** If a warning says the address was autodetected and the
  host has more than one interface, pin it with `--advertise`. This is the most
  common cause of a node that serves correctly and never joins its peers.
- **`configuration file: ...`** Which file was actually read, if any. A
  configuration change that had no effect is usually a different file.

Sockets are reserved before the models load, so a port conflict fails in the
first second rather than after a multi-minute warmup. Between reservation and
serving, the ports are open but silent, and a healthcheck sees a refused
connection. That is the honest answer for "still starting".

## 6. Confirm it serves

```bash
postvec-server status
curl -sk https://127.0.0.1:22222/ready
```

:::: tip Expected
`status` prints the version, the engine root, the advertised addresses and one
line per loaded model. `/ready` answers `200` once a model can serve a
prediction, and `503` before that. Boot warmup runs one throwaway prediction
per embedding model, so `/ready` means "answers at steady-state latency"
rather than "has finished loading".
::::

Next: [connect PostgreSQL](/docs/server/connect).

## Related documentation

- [Models on a node](/docs/server/models) - pull, activate and load
- [Node reference](/docs/server/reference) - every flag, and troubleshooting
- [Packages](/docs/install/packages) - where the runtime and model come from
