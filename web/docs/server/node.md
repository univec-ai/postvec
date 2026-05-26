---
title: Run a node
description: Install the files, choose TLS or --insecure, start postvec-server and read its boot log.
---

# Run a node

One node, from an empty directory. The result is a process serving gRPC on
`33333` and discovery on `22222`, with at least one model loaded.

## 1. Install the package

`postvec-server` is published with every release as a `.deb` / `.rpm`, one
per distribution and architecture (there is no PostgreSQL major in it), next
to the runtime and model packages it serves. Download and verify them as on
the [packages page](/docs/install/packages), then:

<PgSnippet id="packages-server" />

The package installs the binary, the systemd unit, a conffile at
`/etc/postvec-server/config.json` and the `postvec-server` service account.
It enables and starts nothing — the certificate and `systemctl` lines are
yours. `postvec-cli` is a Recommends of the node package (for `postvec model
pull`); it is listed explicitly because an install from local files cannot
fetch a recommended package by itself.

::: warning Licence
`postvec-server` is not under the PostgreSQL License that covers the rest of
postvec. The package's copyright file, the image label and the release
manifest state its terms; see the [release page](/download#postvec-server).
:::

A checkout still builds it, and needs neither pgrx nor PostgreSQL headers:

```bash
cargo build --release -p postvec-server
sudo install -m 0755 target/release/postvec-server /usr/local/bin/
```

## 2. Put the files in place

A node needs ONNX Runtime and at least one model under the same root. The
packaged unit reads **`/opt/postvec`**, which is exactly where these packages
install and where `postvec model pull` writes — so the install line above
already put everything in place:

| Artifact | Installs |
|---|---|
| `postvec-onnxruntime` | `libonnxruntime.so` under `/opt/postvec/libs` |
| `postvec-model-minilm-l6-v2` | The bundled 384-d model under `/opt/postvec/models` |
| `postvec-extras` | Pins the two above |
| `postvec-cli` | `/usr/bin/postvec`, for `model` and `provider` commands (a Recommends of the node; the daemon itself never needs it) |

A node built from a checkout, or a unit without the packaged drop-in, defaults
to `/var/lib/postvec-server` instead: point `--root` at `/opt/postvec`, or copy
the tree there to keep the node's inventory separate from any local cluster.
See [models on a node](/docs/server/models) for pulling more.

## 3. Decide about TLS

The discovery listener requires TLS. Without `--insecure` and without a
readable certificate pair, the node **refuses to start**.

Operators write `POSTVEC_HTTP_ENDPOINTS=https://...` into a cluster. A
silent fallback to plain HTTP would leave a listener that answers and a
client that fails.

Self-signed certificates are accepted. The trust boundary is the network.

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
found without flags — for a node run from a checkout. **On a package
install** the engine root is `/opt/postvec`, root-owned and read-only for the
service account, so the packaged configuration names the pair beside itself
instead: `/etc/postvec-server/server.crt` and `server.key`, installed
`root:postvec-server`, the key `0640`. The package's post-install message
prints the two `install` lines.

## 4. Start it

### As a container

The published image is the packages above composed on Debian 12: the node,
the CLI, ONNX Runtime and the bundled model — the same model bytes the
complete postvec image runs in-process. It serves MiniLM out of the box:

<PgSnippet id="docker-server" />

The container generates its own self-signed certificate at start, per
container, never baked into the image. Mount a pair over
`/etc/postvec-server/server.crt` and `server.key` (where the packaged
configuration looks) or pass `--ssl-cert` / `--ssl-cert-key` to override it. The healthcheck is
`/ready`, so `docker inspect` reports healthy only once a model can answer.
The admin port is not exposed. Verify the image the way you verify a package:

<PgSnippet id="docker-server-verify" />

A checkout builds the same image from locally built packages with
`packaging/postvec/scripts/build-server-image.sh`.

### As a service

The package installs the unit from `postvec-server/systemd/` plus a drop-in
that sets `POSTVEC_SERVER_ROOT=/opt/postvec`. It runs the process
unprivileged, with a strict sandbox and a read-only model root:

```bash
sudo systemctl enable --now postvec-server
journalctl -u postvec-server -f
```

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

Check these two lines:

- **`advertising ...`** If a warning says the address was autodetected and the
  host has more than one interface, pin it with `--advertise`. Otherwise a
  node can serve locally and never join its peers.
- **`configuration file: ...`** The file that was actually read. A change
  that had no effect usually hit a different file.

Sockets are reserved before models load, so a port conflict fails immediately.
Between reservation and serving the ports are open but silent; a healthcheck
sees a refused connection while the node is still starting.

## 6. Confirm it serves

```bash
postvec-server status
curl -sk https://127.0.0.1:22222/ready
```

:::: tip Expected
`status` prints the version, the engine root, the advertised addresses and one
line per loaded model. `/ready` answers `200` once a model can serve a
prediction, and `503` before that. Boot warmup runs one request per
embedding model, so `/ready` means the node answers at normal latency.
::::

Next: [connect PostgreSQL](/docs/server/connect).

## Related documentation

- [Models on a node](/docs/server/models) - pull, activate and load
- [Node reference](/docs/server/reference) - every flag, and troubleshooting
- [Packages](/docs/install/packages) - where the runtime and model come from
