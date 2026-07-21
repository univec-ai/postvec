---
title: Embedded vs remote
description: Operational differences between embedded inference and remote gRPC inference.
---

# Embedded vs remote

Embedded is the default: inference on the database host. Remote mode
runs inference on `postvec-server` nodes. Use remote to isolate
inference from PostgreSQL (crash domain, CPU, GPU) or to share one
engine across databases.

<figure class="pvd">
<svg viewBox="0 0 632 368" role="img" aria-labelledby="pvd-modes-title pvd-modes-desc">
<title id="pvd-modes-title">Where the inference engine sits in each mode</title>
<desc id="pvd-modes-desc">Two topologies with the worker and the engine in the same positions. In embedded mode the PostgreSQL process boundary encloses both, so an engine fault takes the database process with it. In remote mode the boundary encloses only the worker and the engine runs as a separate postvec-server process reached over gRPC.</desc>
<defs>
<marker id="pvd-md-head" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
<polygon class="head" points="0 0, 8 3, 0 6"/>
</marker>
</defs>

<rect class="s-mask" width="632" height="368"/>

<rect class="s-zone" x="24" y="56" width="584" height="112" rx="8"/>
<rect class="s-mask" x="64" y="50" width="164" height="12"/>
<text class="t-zone" x="146" y="59" text-anchor="middle">EMBEDDED · POSTGRESQL PROCESS</text>

<rect class="s-zone" x="24" y="224" width="280" height="112" rx="8"/>
<rect class="s-mask" x="64" y="218" width="152" height="12"/>
<text class="t-zone" x="140" y="227" text-anchor="middle">REMOTE · POSTGRESQL PROCESS</text>

<path class="c" d="M 240,116 H 400" marker-end="url(#pvd-md-head)"/>
<path class="c" d="M 240,284 H 400" marker-end="url(#pvd-md-head)"/>

<rect class="s-mask" x="296" y="96" width="60" height="12"/>
<text class="t-arrow" x="326" y="105" text-anchor="middle">LOOPBACK</text>
<rect class="s-mask" x="304" y="264" width="40" height="12"/>
<text class="t-arrow" x="324" y="273" text-anchor="middle">gRPC</text>

<rect class="s-mask" x="64" y="88" width="176" height="56" rx="6"/>
<rect class="s-node" x="64" y="88" width="176" height="56" rx="6"/>
<text class="t-name" x="152" y="112" text-anchor="middle">Worker</text>
<text class="t-sub" x="152" y="126" text-anchor="middle">in the launcher</text>

<rect class="s-mask" x="400" y="88" width="176" height="56" rx="6"/>
<rect class="s-focal" x="400" y="88" width="176" height="56" rx="6"/>
<text class="t-name" x="488" y="112" text-anchor="middle">Engine</text>
<text class="t-sub" x="488" y="126" text-anchor="middle">same process</text>

<rect class="s-mask" x="64" y="256" width="176" height="56" rx="6"/>
<rect class="s-node" x="64" y="256" width="176" height="56" rx="6"/>
<text class="t-name" x="152" y="280" text-anchor="middle">Worker</text>
<text class="t-sub" x="152" y="294" text-anchor="middle">in the launcher</text>

<rect class="s-mask" x="400" y="256" width="176" height="56" rx="6"/>
<rect class="s-focal" x="400" y="256" width="176" height="56" rx="6"/>
<text class="t-name" x="488" y="280" text-anchor="middle">postvec-server</text>
<text class="t-sub" x="488" y="294" text-anchor="middle">its own process</text>
</svg>
</figure>

`postvec.mode` is cluster-wide and POSTMASTER. Changing it requires a
restart. SQL, the job queue, retry policy and the gRPC wire contract
stay the same.

| | Embedded | Remote (`grpc`) |
|---|---|---|
| Inference | One engine inside the PostgreSQL launcher | `postvec-server` nodes you operate |
| Discovery | Loopback HTTP on the launcher | `GET /config` on those nodes |
| DB-host assets | Extension, CLI, ONNX Runtime, models | Extension + CLI |
| Raw text leaves the DB host | Stays on the host, except columns bound to an [external provider](/docs/models/providers) | Goes to the configured nodes. Provider-bound columns continue to the hosted provider |
| Model commands | `postvec model pull / upgrade / rm / activate` | On each node: CLI or [dashboard](/docs/server/dashboard) |
| Engine crash | Restarts the launcher | Isolated in the node process |
| Dashboard | | Port `22222` on the node |
| Typical use | Single-node, private, edge, air-gapped, regulated | Crash-domain isolation, GPU, one engine shared by several databases |

The same package supports both modes. `setup --embedded` (the default)
selects embedded; `setup --grpc` / `--http` selects remote.

## Embedded

```bash
sudo postvec setup --database app --embedded

postvec model ls
sudo postvec doctor --database app --deep
```

The engine root must contain:

```
{root}/libs/**/libonnxruntime.so     # unversioned filename required
{root}/models/{backend}/{name}/      # descriptor and weights
```

Workers keep jobs pending until the engine listener is ready. `model
pull` installs files deactivated; `model activate` is what loads them.

The embedded gRPC/HTTP listeners stay on **127.0.0.1**.

Local inference, weights and text remain on the database host. A column bound
to a hosted provider sends text through the embedded inference host.

## Remote gRPC

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222
```

On each inference host, the same command on every node:

```bash
postvec-server --root /var/lib/postvec-server \
  --peers node-1,node-2,node-3 \
  --ssl-cert /etc/postvec-server/tls.crt \
  --ssl-cert-key /etc/postvec-server/tls.key
```

Every node must carry the same enabled models. postvec round-robins the
endpoints it was given, so a converter present on some nodes and not
others fails intermittently. `postvec-server status --fleet` is the
check.

`postvec-server` is a CPU or GPU inference node on the deployment
network. Every node in a fleet runs the same command and binds the same
ports; models are files on disk that each node loads. The node also
serves a [dashboard](/docs/server/dashboard) on the discovery port.
Running one node, and running several, is [remote inference](/docs/server/).

The gRPC port is plaintext and unauthenticated by design, so the nodes
belong on a trusted private network. [External provider](/docs/models/providers)
credentials, when required, stay on that side. Every node must carry the
same connector files and the same models. A UniVec hosted converter sends
stored vectors from the node to UniVec. `--allow-unreachable` is only for
staging configuration before the nodes exist.

If the remote engine is unavailable, jobs remain **pending** without
consuming retry attempts. Lexical search can still run while
`postvec.search_degrade_to_fts` is on (the default).

## Switching

Mode is cluster-wide. Switching requires every configured database name
and the `--switch-mode` flag:

```bash
sudo postvec setup --database app \
  --embedded --switch-mode
```

Engine files already on the database host stay in place in remote mode.
