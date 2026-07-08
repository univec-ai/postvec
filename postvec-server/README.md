# postvec-server

The inference node postvec's **remote** mode dials.

postvec has two shapes. Embedded mode — the default — hosts an inference
engine inside the PostgreSQL launcher process: one container, no API key,
nothing to operate. In remote mode (`postvec.mode = 'grpc'`) that engine lives
somewhere else and PostgreSQL is a thin client, which is what you want once
inference should not share a crash domain, a CPU budget or a memory budget
with the database, or once several databases should share one GPU.

`postvec-server` is that somewhere else.

```console
postvec-server --root /var/lib/postvec-server --insecure
```

```sql
-- on the database host
SELECT postvec.enable('public.docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
```
```console
postvec setup --database mydb \
  --grpc 10.0.0.10:33333 \
  --http https://10.0.0.10:22222
```

Same SQL, same queue, same wire contract as embedded mode. The only thing that
changes is where the model runs.

> **License.** This directory is under the Business Source License 1.1
> (source-available), not the PostgreSQL License the rest of the repository
> uses. Production use by an organization needs a commercial license.
> See [LICENSE](LICENSE) and the root [LICENSE](../LICENSE) index.

## What it is not

- **Not a model downloader.** There is no hub, no S3 client, no JIT fetch. A
  model that is not on disk is a refusal. Put models there with
  `postvec model pull`, a shared volume, or whatever copy step you already
  have.
- **Not a public API.** No authentication and no billing. HTTP
  (`/api/{model}`, `/api/openai/embeddings`) is there so a dashboard and an
  OpenAI-shaped client can talk to the same engine the database already
  uses. It is still dialed on a trusted private network, exactly as the
  extension's GUC help says.
- **Not a control plane.** Every node is identical, binds the same ports and
  runs the same command. Gossip answers one question — who else is alive — and
  replicates nothing.

## Ports

| Port | Protocol | Bound to | Purpose |
|---|---|---|---|
| `33333` | gRPC, plaintext | `--bind` (default `0.0.0.0`) | `EmbedTexts`, `ConvertEmbeddings` |
| `22222` | HTTPS | `--bind` | `/config`, `/health`, `/ready`, `/metrics`, `/api/{model}`, `/api/openai/embeddings`, optional UI |
| `11111` | TCP gossip | all interfaces | cluster membership |
| `22223` | HTTP | `127.0.0.1` only | `/admin/load`, `/admin/unload` |

Identical on every node in a fleet — that is the point, and it is why
`--peers` takes bare hosts.

**The gRPC and discovery ports have no authentication.** postvec's gRPC client
speaks plaintext, so adding TLS on this side alone would break every existing
`postvec setup --grpc`. Keep both behind a firewall, a security group, or
`--bind <private-ip>`. Only the loopback admin port can change what the engine
has loaded.

## A fleet

The same command on every node; only `--advertise` differs, and only on a host
with more than one interface.

```console
postvec-server \
  --root /var/lib/postvec-server \
  --peers node-1,node-2,node-3 \
  --advertise 10.0.0.10 \
  --ssl-cert /etc/postvec-server/tls.crt \
  --ssl-cert-key /etc/postvec-server/tls.key
```

Then point the database at all of them:

```console
postvec setup --database mydb \
  --grpc 10.0.0.10:33333,10.0.0.11:33333,10.0.0.12:33333 \
  --http https://10.0.0.10:22222,https://10.0.0.11:22222,https://10.0.0.12:22222
```

postvec round-robins the gRPC list and unions `/config` across the HTTP list.
It never joins gossip and never reads membership — clustering is there so the
fleet can describe itself, not so the extension can discover it.

**Every node must carry the same enabled models.** Nothing replicates them.
postvec will happily route a conversion at a node that does not have that
converter, and the symptom is an intermittent `MODEL_NOT_LOADED` that looks
like a fluke. `postvec-server status --fleet` is the check.

## Configuration

`defaults < config file < environment < flags`. A flag that is absent does not
clear a file value; a flag that is present always wins. No file is required.

```console
postvec-server --help
```

The file is optional JSON, searched at `$root/postvec-server.json`,
`/etc/postvec-server/config.json`, `/etc/postvec-server.json`, then
`./postvec-server.json` — or wherever `--config` points. Unknown keys are a
hard error, because a typo that is ignored produces a node that starts and
then behaves nothing like its configuration. See
[postvec-server.example.json](postvec-server.example.json).

The on-disk layout is identical to embedded mode's: a model root is portable
between the two, and a tree `postvec model pull` already wrote into works
unchanged (name it with `--root` or `POSTVEC_SERVER_ROOT`).

## Operating

```console
postvec-server status              # models, peers, on-disk drift
postvec-server status --fleet      # ... and every peer's inventory
postvec-server load  <model>       # make a pulled model resident, no restart
postvec-server unload <model>
```

All three talk to `127.0.0.1` on the admin port. They are node-local tools:
nothing here mutates a peer. Exit codes: `0` healthy, `1` reachable but
degraded, `2` unreachable.

After `postvec model pull` and `postvec model activate`, a model is on disk but
not resident — `/config` advertises what the node can serve *now*. Run
`postvec-server load <model>` (or restart) to make it visible to discovery.

### HTTP inference

Same engine as gRPC, on the discovery port.

| Route | Body | Response |
|---|---|---|
| `GET /api/{model}` | — | Native envelope with the model's layer overview |
| `POST /api/{model}` | Native JSON (`texts` for embed models, `embeddings` for converters; keys follow `executor.inputs`) | `{success, data}` or `{success, error:{message}}`. HTTP stays 200 so a dashboard can treat the envelope as the contract |
| `POST /api/openai/embeddings` | OpenAI `/v1/embeddings` (`input` as a string or array of strings; optional `encoding_format`, `dimensions`, `input_type`) | OpenAI `{object, data, model, usage}`. Errors use `{error:{message, type, code, param}}` and a real HTTP status: 400 invalid request, 404 `model_not_found`, 504 deadline |

The OpenAI route is an adaptor: it rewrites `input` into the model's first
executor input (`texts`), drops a `postvec/` / `univec/` prefix from `model`,
and runs the same executor path as `POST /api/{model}`. Token-ID inputs and
converters are refused; provider-backed models refuse `dimensions` and
`base64` instead of ignoring them. Any OpenAI SDK works with
`base_url = <node>/api/openai`. Both routes cap a request at 4096 items.

A built dashboard (see [web-ui/](web-ui/)) is served from this port when
`index.html` is found: `--web-ui DIR`, `POSTVEC_SERVER_WEB_UI`, `web_ui` in
the config file, else `<root>/server/ui`, where the packages install it.

### Health

- `/health` — the process is up. Stays 200 through a drain.
- `/ready` — a model can answer right now. 503 before the first model loads
  and for the whole drain, so a load balancer stops sending work before the
  socket closes.
- `/metrics` — Prometheus text. Requests, latency, and errors labelled by the
  same `x-ravenna-error-code` values that cross the wire, which turns "why is
  this column stuck" into a query.

On `SIGTERM` the node flips `/ready` to 503, announces its departure to its
peers, keeps serving for `--drain-delay-ms` (5 s) so both are observed before
the socket closes, then finishes in-flight work and exits. `/health` stays 200
throughout. A second signal skips the wait.

### systemd

```ini
[Service]
Environment=POSTVEC_SERVER_ROOT=/var/lib/postvec-server
Environment=RUST_LOG=info
ExecStart=/usr/bin/postvec-server --config /etc/postvec-server/config.json
Restart=on-failure
```

A ready-made unit is in [systemd/](systemd/).

## Installing

Every postvec release publishes the node as a package and as an image, built
from the same commit as the extension it serves and tested against it.

```console
# A node host (Debian/Ubuntu; .rpm for EL9). The packaged unit reads
# /opt/postvec, which is where postvec-extras — ONNX Runtime plus the bundled
# model — installs, and where `postvec model pull` writes.
sudo apt install ./postvec-server_*.deb ./postvec-cli_*.deb \
  ./postvec-onnxruntime_*.deb ./postvec-model-*.deb ./postvec-extras_*.deb

# The discovery listener needs a certificate pair the service account can
# read; the packaged configuration looks here (the engine root is read-only).
sudo install -o root -g postvec-server -m 0644 server.crt /etc/postvec-server/server.crt
sudo install -o root -g postvec-server -m 0640 server.key /etc/postvec-server/server.key

sudo systemctl enable --now postvec-server
postvec-server status
```

```console
# The same packages as an image, serving the bundled model out of the box.
docker run -d --name postvec-server -p 22222:22222 -p 33333:33333 \
  ghcr.io/univec-ai/postvec-server:0.1.0-1
```

The package installs files and creates the `postvec-server` account; it
enables and starts nothing. Configuration is `/etc/postvec-server/config.json`
(a conffile; the commented example is under `/usr/share/doc/postvec-server/`),
and the unit runs with the sandbox in [systemd/](systemd/) plus a drop-in that
sets `POSTVEC_SERVER_ROOT=/opt/postvec`. `postvec-cli` is a Recommends — the
daemon never calls it, but `postvec model pull` (as root: the engine root is
package-owned) is how more models get onto the node. Verification steps, file
names and the image's tag scheme are on the release page.

## Building

```console
cargo build -p postvec-server --release              # CPU
cargo build -p postvec-server --release --features ort-cuda
```

Needs `protoc` (the wire contract is compiled from `../proto`) and, at
runtime, ONNX Runtime under `<root>/libs/**/libonnxruntime.so` — the
`postvec-onnxruntime` package provides it. GPU builds are a second artifact,
not a different license.

## Contributing

This directory is under the Business Source License 1.1 while its neighbours are
PostgreSQL-licensed, and Univec licenses it commercially, so a patch here needs
a contributor license agreement rather than the DCO. Until that agreement is
published, external pull requests to this directory are not accepted; open an
issue instead.
