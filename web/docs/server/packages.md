---
title: postvec-server packages
description: Install postvec-server from .deb or .rpm, add TLS and start the unit.
---

# Packages

`postvec-server` is published with every release as a `.deb` / `.rpm`,
one per distribution and architecture (no PostgreSQL major). Download
and verify the files as on the [packages page](/docs/install/packages),
then:

<PgSnippet id="packages-server" />

The package installs the binary, the systemd unit, a conffile at
`/etc/postvec-server/config.json`, the dashboard under
`/opt/postvec/server/ui` and the `postvec-server` service account.
`postvec-cli` is a Recommends of the server package (for
`postvec model pull`); it is listed explicitly because an install from
local files cannot fetch a recommended package by itself.

## Files

The packaged unit reads **`/opt/postvec`**, where these packages install
and where `postvec model pull` writes:

| Artifact | Installs |
|---|---|
| `postvec-onnxruntime` | `libonnxruntime.so` under `/opt/postvec/libs` |
| `postvec-model-minilm-l6-v2` | The bundled 384-d model under `/opt/postvec/models` |
| `postvec-extras` | Pins the two above |
| `postvec-cli` | `/usr/bin/postvec`, for `model` and `provider` commands |
| dashboard | `/opt/postvec/server/ui`, served on port `22222` |

## TLS

The discovery listener requires TLS. Without `--insecure` and without a
readable certificate pair, the process exits at start.

Operators write `POSTVEC_HTTP_ENDPOINTS=https://...` into a cluster. A
silent fallback to plain HTTP would leave a listener that answers and a
client that fails.

Self-signed certificates are accepted. The trust boundary is the network.

```bash
postvec-server \
  --root /opt/postvec \
  --ssl-cert /etc/postvec-server/server.crt \
  --ssl-cert-key /etc/postvec-server/server.key
```

The packaged configuration names `/etc/postvec-server/server.crt` and
`server.key`, installed `root:postvec-server`, the key `0640`. The
package's post-install message prints the two `install` lines.

## Start the unit

The package installs the unit from `postvec-server/systemd/` plus a
drop-in that sets `POSTVEC_SERVER_ROOT=/opt/postvec`. It runs
unprivileged, with a strict sandbox and a read-only model root:

```bash
sudo systemctl enable --now postvec-server
journalctl -u postvec-server -f
```

## Boot log

```text
postvec-server 0.1.0 (onnx)
engine root: /opt/postvec
configuration file: /etc/postvec-server/config.json
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

- **`advertising ...`** If a warning says the address was autodetected
  and the host has more than one interface, pin it with `--advertise`.
- **`configuration file: ...`** The file that was actually read.

Sockets are reserved before models load, so a port conflict fails
immediately. Between reservation and serving the ports are open but
silent; a healthcheck sees a refused connection while the process is
still starting.

:::: info Optional
```bash
postvec-server status
curl -sk https://127.0.0.1:22222/ready
```

`status` prints version, engine root, addresses and loaded models.
`/ready` is `200` once a model can serve, `503` before that.
::::

::: warning Licence
`postvec-server` is **Business Source License 1.1** (source-available).
Personal production use, non-production environments and a 30-day
production evaluation per organization are free. Production use by an
organization needs [postvec Pro](https://univec.ai). [License](/docs/license).
:::

Next: [connect PostgreSQL](/docs/server/connect). The dashboard is
`https://<host>:22222`.

- [Docker](/docs/server/docker)
- [From source](/docs/server/source)
- [Dashboard](/docs/server/dashboard)
- [Models](/docs/server/models)
- [Reference](/docs/server/reference)
