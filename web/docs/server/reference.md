---
title: Node reference
description: postvec-server flags, environment and file keys, health routes, metrics and troubleshooting.
outline: deep
---

# Node reference

`postvec-server --help` is the authority. This page is for reading what a
setting does and where it can be set.

## Configuration precedence

**Defaults < config file < environment < flags.** A present flag always wins.
An omitted flag leaves the file or environment value in place. No file is
required.

The file is searched at `$root/postvec-server.json`,
`/etc/postvec-server/config.json`, `/etc/postvec-server.json`, then
`./postvec-server.json`, unless `--config` names one. In that case a missing
file is an error.

Unknown keys are a hard error, and the message names the key. Keys beginning
with `//` are comments.

| Flag | Environment | File key | Default |
|---|---|---|---|
| `--root PATH` | `POSTVEC_SERVER_ROOT` | - | Current directory |
| `--config PATH` | `POSTVEC_SERVER_CONFIG` | - | The search order above |
| `--bind ADDR` | `POSTVEC_SERVER_BIND` | `bind_address` | `0.0.0.0` |
| `--http PORT` | `POSTVEC_SERVER_HTTP` | `http_port` | `22222` |
| `--grpc PORT` | `POSTVEC_SERVER_GRPC` | `grpc_port` | `33333` |
| `--gossip PORT` | `POSTVEC_SERVER_GOSSIP` | `gossip_port` | `11111` |
| `--admin PORT` | `POSTVEC_SERVER_ADMIN` | `admin_port` | `22223` |
| `--peers LIST` | `POSTVEC_SERVER_PEERS`, then `..._CLUSTER` | `peers` (or `cluster`) | Empty, a single node |
| `--group NAME` | `POSTVEC_SERVER_GROUP` | `group` | `postvec` |
| `--advertise IP` | `POSTVEC_SERVER_ADVERTISE` | `advertise` | `--bind` if specific, else autodetected |
| `--frontend URL` | `POSTVEC_SERVER_FRONTEND` | `frontend` | `{scheme}://{advertise}:{http}` |
| `--ssl-cert PATH` | `POSTVEC_SERVER_SSL_CERT` | `ssl.cert` | `$root/certs/server.crt` |
| `--ssl-cert-key PATH` | `POSTVEC_SERVER_SSL_KEY` | `ssl.key` | `$root/certs/server.key` |
| `--insecure` | `POSTVEC_SERVER_INSECURE` | `insecure` | Off. TLS required |
| `--models LIST` | `POSTVEC_SERVER_MODELS` | `models` | Empty, every enabled descriptor |
| `--providers-path PATH` | `POSTVEC_SERVER_PROVIDERS_PATH` | `providers_path` | `$root/providers.d`. A path, never a credential |
| `--predict-timeout-ms N` | `POSTVEC_SERVER_PREDICT_TIMEOUT_MS` | `predict_timeout_ms` | `30000` |
| `--max-inflight N` | `POSTVEC_SERVER_MAX_INFLIGHT` | `max_inflight` | CPU count, clamped to 4-16 |
| `--max-resident-models N` | `POSTVEC_SERVER_MAX_RESIDENT_MODELS` | `max_resident_models` | `16` |
| `--drain-delay-ms N` | `POSTVEC_SERVER_DRAIN_DELAY_MS` | `drain_delay_ms` | `5000` |
| `--no-warmup` | `POSTVEC_SERVER_WARMUP=0` | `warmup` | Warmup on |
| `--no-metrics` | `POSTVEC_SERVER_METRICS=0` | `metrics` | Metrics on |
| `--log-level FILTER` | `RUST_LOG` | `log_level` | `info` |
| `--web-ui DIR` | `POSTVEC_SERVER_WEB_UI` | `web_ui` | `<root>/server/ui`, where the package installs it |

`--cluster` is an accepted alias for `--peers`, and `--ssl-key` for
`--ssl-cert-key`.

### The two knobs that matter

**`--max-inflight`** bounds concurrently executing predictions. It is a CPU
bound, because each ONNX session runs its own intra-op thread pool and N
concurrent predictions can oversubscribe an N-core box several times over. It
is also a memory bound, because each in-flight request may hold a response
tree up to the transport envelope. Raise it on a large node when latency is
fine and throughput is not. Lower it when the box is shared.

**`--max-resident-models`** bounds how many models the engine holds, counting
whole dependency closures: a bridge model pulls its converters in with it.
Resident models are the dominant memory cost on a node. Exceeding the ceiling
is refused at boot, before any model loads.

### `frontend`

The reachable HTTP URL of a node is not always
`https://{advertise}:{http_port}`. NAT, a load balancer or a DNS name you
would rather see printed all break that assumption, and `--frontend`
overrides what `/config` and `postvec-server status` display and what peers
learn.

gRPC uses the advertised IP, membership uses the gossip socket, and
postvec's discovery ignores `frontend`. Set `--advertise` (and
`--frontend` when the printed URL should differ) to the address peers
and clients actually use.

## Health routes

| Route | Meaning |
|---|---|
| `GET /health` | The process is up. Stays `200` through a drain |
| `GET /ready` | A model can answer **right now**. `503` before the first model loads, and for the whole drain |
| `GET /config` | Model discovery, which is what postvec reads. Also carries `server`, `cluster` and `system` blocks |
| `GET /metrics` | Prometheus text |
| `GET /api/{model}` | Model layer overview, native envelope |
| `POST /api/{model}` | Native inference. JSON body keyed like `executor.inputs` (`texts` / `embeddings`). Envelope `{success, data}` or `{success, error:{message}}`; HTTP stays 200 |
| `POST /api/openai/embeddings` | OpenAI `/v1/embeddings` adaptor in front of the native path. `{object, data, model, usage}` on success; `{error:{message, type, code}}` and a real status on failure. See [HTTP API](/docs/server/http-api) |

Gate load balancers and compose healthchecks on `/ready`, and supervisors on
`/health`. Confusing the two produces either a node that receives traffic
before it can serve it, or a supervisor that restarts a node mid-drain.

Boot warmup runs one request per embedding model, so `/ready` means the
node answers at normal latency.
`--no-warmup` skips it, and `postvec_server_warmup_failures_total` counts what
went wrong.

## Metrics worth alerting on

| Series | Why |
|---|---|
| `postvec_server_ready` | `0` for longer than a restart takes means the node is not coming back |
| `postvec_server_request_errors_total{code="MODEL_NOT_LOADED"}` | Almost always inventory drift across the fleet |
| `postvec_server_request_errors_total{code="TIMEOUT"}` | Requests exceeding their budget. Raise `--max-inflight`, add nodes, or accept the latency |
| `postvec_server_request_duration_seconds` | The p99 your database's `search()` inherits |
| `postvec_server_models_enabled_on_disk` > `..._models_loaded` | Something was pulled or activated and never loaded |
| `postvec_server_cluster_members` | Below the fleet size means a partition or a wrong `--advertise` |

Error codes are the same values that cross the wire and drive postvec's retry
and dead-letter policy.

## Security

**The gRPC and discovery ports are unauthenticated.** gRPC is also plaintext.
postvec's client speaks plaintext and the extension's own setting help says
so, so adding TLS on the server side alone would break every existing
`postvec setup --grpc`.

Restrict both ports to a private network with a firewall, a security group
or `--bind <private-ip>`. The node logs a warning at boot whenever it binds
every interface.

**Only the loopback admin port can change what is loaded.** `/admin/load`,
`/admin/unload` and `/admin/providers/reload` live on their own socket bound
to `127.0.0.1`, never on the published one. A routable admin bind is a
boot failure. A per-request loopback peer check sits behind that. The
trust boundary is local OS users.

No telemetry and no licence check. Models arrive on disk by whatever
mechanism you choose. An [external provider](/docs/models/providers)
connector is the exception: the node then calls that provider's API for
the models it declares. A UniVec converter sends stored vectors; an
embed entry sends text.

## Troubleshooting

| Symptom | Meaning or next action |
|---|---|
| A node sees only itself | The advertised address. Check the boot log's autodetection warning, pin `--advertise`, then open the gossip port |
| `MODEL_NOT_LOADED`, intermittently | Inventory drift. `postvec-server status --fleet` names the model and the nodes missing it |
| `MODEL_NOT_LOADED`, always, for a model you just pulled | On disk but not resident. `postvec-server load NAME` |
| Refuses to start with a TLS error | No readable certificate pair and no `--insecure` |
| "cannot initialise ONNX Runtime" | No `libonnxruntime.so` under `$root/libs`. Install `postvec-onnxruntime` or point `--root` at a tree that has one |
| A configuration change had no effect | The boot log's `configuration file:` line names the file that was read. Flags beat both the environment and the file |
| Refuses to start over a configuration key | Unknown keys are fatal by design. The error names the key; prefix it with `//` if you meant a comment |
| Requests queue and time out under load | `postvec_server_requests_in_flight` sitting at `--max-inflight` with rising `TIMEOUT`. Raise it if the box has headroom, or add nodes. Lowering `--predict-timeout-ms` makes the failures faster, not fewer |

## Related documentation

- [Run a node](/docs/server/node) - first start and the boot log
- [Run a fleet](/docs/server/fleet) - parity, drift and drains
- [Troubleshooting](/docs/troubleshooting) - the database side
