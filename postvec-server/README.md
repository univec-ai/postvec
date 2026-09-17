# postvec-server

Inference node for postvec's **remote** mode (`postvec.mode = 'grpc'`).

Embedded mode, the default, hosts the engine inside the PostgreSQL launcher
process. Remote mode moves that engine to a separate process so inference has
its own crash domain, CPU budget or GPU, and so several databases can share
one fleet. `postvec-server` is that process.

Docs: [postvec.dev/docs/server](https://postvec.dev/docs/server/).

```console
postvec-server --insecure
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

SQL, queue and wire contract match embedded mode. The model runs here.

> **License.** This directory is under the Business Source License 1.1
> (source-available). The rest of the repository uses the PostgreSQL License.
> Development, testing, personal use and a 30-day production evaluation are
> free. Production use by an organization needs a
> [postvec Pro](https://univec.ai) subscription or an enterprise agreement.
> Each version converts to the PostgreSQL License four years after release.
> See [LICENSE](LICENSE) and the [LICENSING.md](../LICENSING.md) index.

## Scope

Registry pulls accept the signed UniVec catalogue. Models also arrive through
`postvec model pull`, a shared volume or another deployment step. The engine
loads from local disk.

HTTP (`/api/{model}`, `/api/openai/embeddings`) lets a dashboard and an
OpenAI-shaped client talk to the same engine the database already uses. Put
the listener on a trusted private network. The gRPC and discovery ports have
no authentication. postvec's gRPC client speaks plaintext, so TLS on this
side alone would break `postvec setup --grpc`. Bind `--bind <private-ip>` or
place both ports behind a firewall.

Every node is identical: same ports, same command. Gossip answers who else is
alive. It replicates no models and no configuration.

## Ports

| Port | Protocol | Bound to | Purpose |
|---|---|---|---|
| `33333` | gRPC, plaintext | `--bind` (default `0.0.0.0`) | `EmbedTexts`, `ConvertEmbeddings` |
| `22222` | HTTPS | `--bind` | `/config`, `/health`, `/ready`, `/metrics`, `/api/{model}`, `/api/openai/embeddings`, `/api/registry/*`, optional UI |
| `11111` | TCP gossip | all interfaces | cluster membership |
| `22223` | HTTP | `127.0.0.1` only | `/admin/load`, `/admin/unload`, registry pull/activate/deactivate |

A fleet uses the same ports on every node, which is why `--peers` takes bare
hosts. By default only the loopback admin port can change what the engine has
loaded. `--manage` also exposes registry mutations to the dashboard.

## A fleet

The same command on every node. `--advertise` differs only on a host with
more than one interface.

```console
postvec-server \
  --peers node-1,node-2,node-3 \
  --advertise 10.0.0.10 \
  --ssl-cert /etc/postvec-server/tls.crt \
  --ssl-cert-key /etc/postvec-server/tls.key
```

Point the database at all of them:

```console
postvec setup --database mydb \
  --grpc 10.0.0.10:33333,10.0.0.11:33333,10.0.0.12:33333 \
  --http https://10.0.0.10:22222,https://10.0.0.11:22222,https://10.0.0.12:22222
```

postvec round-robins the gRPC list and unions `/config` across the HTTP list.
The extension uses those two lists. Gossip is how the nodes describe the
fleet to each other.

Every node must carry the same enabled models. postvec will route a
conversion at a node that is missing that converter, and the symptom is an
intermittent `MODEL_NOT_LOADED`. `postvec-server status --fleet` is the check.

## Configuration

Precedence: defaults, then config file, then environment, then flags. A
present flag always wins. An absent flag leaves the file value in place.

```console
postvec-server --help
```

The file is optional JSON, searched at `$root/postvec-server.json`,
`/etc/postvec-server/config.json`, `/etc/postvec-server.json`, then
`./postvec-server.json`, or wherever `--config` points. Unknown keys are a
hard error so a typo cannot start a node that ignores its configuration. See
[postvec-server.example.json](postvec-server.example.json).

The on-disk layout matches embedded mode. A model root is portable between
the two, and a tree `postvec model pull` already wrote into works unchanged
(name it with `--root` or `POSTVEC_SERVER_ROOT`).

## Operating

```console
postvec-server status              # models, peers, on-disk drift
postvec-server status --fleet      # ... and every peer's inventory
postvec-server load  <model>       # make a pulled model resident, no restart
postvec-server unload <model>
```

All three talk to `127.0.0.1` on the admin port. They are node-local: they
leave peers unchanged. Exit codes: `0` healthy, `1` reachable but degraded,
`2` unreachable.

After `postvec model pull` and `postvec model activate`, a model is on disk.
`/config` advertises what the node can serve *now*. Run
`postvec-server load <model>` (or restart) to make it visible to discovery.

### HTTP inference

Same engine as gRPC, on the discovery port.

| Route | Body | Response |
|---|---|---|
| `GET /api/{model}` | - | Native envelope with the model's layer overview |
| `POST /api/{model}` | Native JSON (`texts` for embed models, `embeddings` for converters; keys follow `executor.inputs`) | `{success, data}` or `{success, error:{message}}`. HTTP stays 200 so a dashboard can treat the envelope as the contract |
| `POST /api/openai/embeddings` | OpenAI `/v1/embeddings` (`input` as a string or array of strings; optional `encoding_format`, `dimensions`, `input_type`, `user`) | OpenAI `{object, data, model, usage}`. Errors use `{error:{message, type, code, param}}` and a real HTTP status: 400 invalid request, 404 `model_not_found`, 504 deadline |

The OpenAI route is an adaptor: it rewrites `input` into the model's first
executor input (`texts`), drops a `postvec/` / `univec/` prefix from `model`,
and runs the same executor path as `POST /api/{model}`. Token-ID inputs and
converters are refused. Provider-backed models refuse `dimensions` and
`base64` instead of ignoring them. Any OpenAI SDK works with
`base_url = <node>/api/openai`. Both routes cap a request at 4096 items.

### Model registry

The node-side counterpart of `postvec model ls / ls --available / pull /
activate / deactivate`, on the same code the CLI runs. Requests and replies
are the admin envelope: `{success, data}`, per-model
`{model, status, error?}` results.

| Route | Listener | Does |
|---|---|---|
| `GET /api/registry/models` | all | Installed models: `enabled`, `loaded`, `owner`, `revision`, size |
| `GET /api/registry/available` | all | The registry catalogue, with `installed` / `update` per entry. Anonymous, the node's own credential (`POSTVEC_API_KEY`, the service account's `postvec login`), or an `Authorization: Bearer <api key>` sent with the request (the dashboard's per-tab key) |
| `POST /api/registry/pull` `{models, accept_license?}` | admin (`--manage`: all) | Download, verify and install (deactivated, like the CLI); returns a `job` id |
| `GET /api/registry/pulls` | all | Every pull started here: status, bytes, per-model results |
| `POST /api/registry/activate` `{models}` | admin (`--manage`: all) | Enable on disk (with deactivated dependencies), then load |
| `POST /api/registry/remove` `{models}` | admin (`--manage`: all) | Unload, then delete a registry-installed model. Package and manual models stay. |
| `POST /api/registry/deactivate` `{models}` | admin (`--manage`: all) | Unload, then disable. Refused while an enabled model depends on it. |

The three mutating routes stay on the loopback admin port by default.
`--manage` (`POSTVEC_SERVER_MANAGE=1`, `"manage": true`) also exposes them
on the public port so the dashboard can drive them. They are unauthenticated
like the rest of this listener, so use that flag on a trusted network. Any
installed model can be activated or deactivated, however it arrived. The
packaged unit hands `/opt/postvec/models` to the service account at start so
pulls work; the image does the same.

A built dashboard (see [web-ui/](web-ui/)) is served from this port when
`index.html` is found: `--web-ui DIR`, `POSTVEC_SERVER_WEB_UI`, `web_ui` in
the config file, else `<root>/server/ui`, where the packages install it.

### Health

- `/health` - the process is up. Stays 200 through a drain.
- `/ready` - a model can answer right now. 503 before the first model loads
  and for the whole drain, so a load balancer stops sending work before the
  socket closes.
- `/metrics` - Prometheus text. Requests, latency and errors labelled by the
  same `x-ravenna-error-code` values that cross the wire.

On `SIGTERM` the node flips `/ready` to 503, announces its departure to its
peers, keeps serving for `--drain-delay-ms` (5 s) so both are observed before
the socket closes, then finishes in-flight work and exits. `/health` stays 200
throughout. A second signal skips the wait.

### systemd

```ini
[Service]
Environment=POSTVEC_SERVER_ROOT=/opt/postvec
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
# /opt/postvec, which is where postvec-extras - ONNX Runtime plus the bundled
# model - installs, and where `postvec model pull` writes.
sudo apt install ./postvec-server_*.deb ./postvec-cli_*.deb \
  ./postvec-onnxruntime_*.deb ./postvec-model-*.deb ./postvec-extras_*.deb

# The discovery listener needs a certificate pair the service account can
# read; the packaged configuration looks here.
sudo install -o root -g postvec-server -m 0644 server.crt /etc/postvec-server/server.crt
sudo install -o root -g postvec-server -m 0640 server.key /etc/postvec-server/server.key

sudo systemctl enable --now postvec-server
postvec-server status
```

```console
# The same packages as an image, serving the bundled model.
docker run -d --name postvec-server -p 22222:22222 -p 33333:33333 \
  ghcr.io/univec-ai/postvec-server:0.1.0-1
```

The package installs files and creates the `postvec-server` account. Start
the unit yourself. Configuration is `/etc/postvec-server/config.json` (a
conffile; the commented example is under `/usr/share/doc/postvec-server/`).
The unit runs with the sandbox in [systemd/](systemd/) plus a drop-in that
sets `POSTVEC_SERVER_ROOT=/opt/postvec`. `postvec-cli` is a Recommends.
`postvec model pull` as root writes additional models into the package-owned
engine root. Verification steps,
file names and the image's tag scheme are on the release page.

## Building

```console
cargo build -p postvec-server --release              # CPU
cargo build -p postvec-server --release --features ort-cuda
```

Needs `protoc` (the wire contract is compiled from `../proto`) and, at
runtime, ONNX Runtime under `<root>/libs/**/libonnxruntime.so` - the
`postvec-onnxruntime` package provides it. GPU builds are a second artifact
under the same licence.

## Contributing

This directory is under the Business Source License 1.1. Univec licenses it
commercially, so a patch here needs a signed [CLA](../CLA.md). Email
legal@univec.ai, wait for written confirmation, then open the pull request.
The DCO covers the PostgreSQL-licensed crates.
