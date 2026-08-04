---
title: HTTP API
description: Query a node over HTTP with the native contract or an OpenAI-compatible one, and the registry routes the dashboard uses.
---

# HTTP API

Every node answers inference over HTTP on its discovery port (`22222`), with
the same engine and the same models the database reaches over gRPC. The
[dashboard](/docs/server/dashboard) is the interactive form of these
routes. Two contracts are served:

| Route | Contract |
|---|---|
| `POST /api/{model}` | Native. The request body is keyed like the model's `executor.inputs`; the reply is the `{success, data}` envelope. Works for every model type, converters included. |
| `POST /api/openai/embeddings` | OpenAI `/v1/embeddings`. Embed models only. |
| `GET /api/{model}` | The model's input and output layers, native envelope. |

Neither route is authenticated. Like gRPC, they are meant for a private
network; see [security](/docs/security).

## Native

The body keys follow the model descriptor. An embed model takes `texts`, a
converter takes `embeddings`:

```console
curl -sk https://10.0.0.10:22222/api/baai-bge-m3 \
  -H 'content-type: application/json' \
  -d '{"texts": ["quarterly revenue guidance was raised"]}'
```

```json
{"success": true, "data": {"embeddings": [[0.0123, ...]], "usage": {"prompt_tokens": 9, "total_tokens": 9}}}
```

Embed models also accept `dimensions` (Matryoshka truncation, refused above
the model's `target_dim`), `encoding_format` (`float` or `base64`) and
`input_type` (`search_query` / `search_document`, applied only when the model
ships templates for it). Conversion:

```console
curl -sk https://10.0.0.10:22222/api/convert-baai_bge_m3-to-cohere_embed_v4.0 \
  -H 'content-type: application/json' \
  -d '{"embeddings": [[0.0123, ...]]}'
```

Errors keep HTTP `200` and set `success: false`, the way the dashboard and
the ninference tooling expect:

```json
{"success": false, "error": {"message": "model \"x\" is not loaded on this node; ..."}}
```

A path that is not a route is `404`. Load the model first with
[`postvec-server load`](/docs/server/models) or from the
[dashboard](/docs/server/dashboard).

## OpenAI-compatible

Point any OpenAI SDK at `/api/openai` as its base URL. The API key is not
checked, but the SDKs insist on one:

```python
from openai import OpenAI

client = OpenAI(base_url="https://10.0.0.10:22222/api/openai", api_key="unused")
reply = client.embeddings.create(model="baai-bge-m3", input=["hello", "world"], dimensions=256)
```

The route rewrites `input` into the native payload, runs the same executor as
`POST /api/{model}`, and reshapes the result into
`{object: "list", data: [{object, index, embedding}], model, usage}`. `model`
is echoed verbatim; a `postvec/` or `univec/` prefix is accepted and ignored
for resolution. `encoding_format`, `dimensions` and `input_type` pass through.

Errors carry a real status and the OpenAI `{error: {message, type, code,
param}}` shape, so SDK exceptions are typed:

| Case | Status | `type` / `code` |
|---|---|---|
| Malformed body, token-ID input, a converter as `model` | `400` | `invalid_request_error` |
| `dimensions` above the model width, input over the sequence length | `400` | `invalid_request_error` / `context_length_exceeded` |
| Model not loaded on this node | `404` | `invalid_request_error` / `model_not_found` |
| Execution past the node's deadline | `504` | `api_error` |
| Provider upstream rejected the key / unavailable | `401` / `502` | `authentication_error` / `api_error` |

Token IDs as `input` are refused: the node tokenizes text itself.

## Provider-backed models

Models served through an [external provider](/docs/models/providers) answer both
routes too. They have no post-processing of their own, so `dimensions` and
`encoding_format: base64` are refused for them with a `400` rather than
silently ignored, and `usage` reports zeros.

## Model registry

The node can also manage its own models, the way `postvec model` does on a
host: list what is installed, browse the registry catalogue, pull, activate
and deactivate. Same code, same on-disk result, same receipts.

| Route | Body | Reply |
|---|---|---|
| `GET /api/registry/models` | | Installed models with `enabled`, `loaded`, `owner`, `removable`, `revision`, `disk_bytes` |
| `GET /api/registry/available` | | The catalogue, each entry with `installed` and `update` (`not-installed`, `current`, `upgradable`, `unknown`) |
| `POST /api/registry/pull` | `{"models": [...], "accept_license": ["<license>@<version>"]}` | `{"job": id}`; the pull runs on the node |
| `GET /api/registry/pulls` | | Every pull: `status`, `downloaded_bytes`, `total_bytes`, per-model `results` |
| `POST /api/registry/activate` | `{"models": [...]}` | Per-model results; enables on disk (deactivated dependencies too) and loads |
| `POST /api/registry/deactivate` | `{"models": [...]}` | Per-model results; unloads and disables |
| `POST /api/registry/remove` | `{"models": [...]}` | Per-model results; unloads and deletes registry-installed models |

Like the CLI, a pull installs a model **deactivated**; activating is what
turns it on and loads it. Pulls run one at a time; a second request queues.
A model another enabled model depends on cannot be deactivated or removed. Any installed model can be activated or deactivated, whether it
came from the registry, a package or a copy.
Package-owned and manually managed models cannot be removed through HTTP; use
the package manager or remove the manual directory deliberately.

The `GET` routes are on every listener. The `POST` routes are on the
loopback admin port (`22223`), and on the public port when the node
runs with `--manage` (`POSTVEC_SERVER_MANAGE=1`, `"manage": true`).
Those POSTs are unauthenticated; `--manage` belongs on a private
network.

The catalogue is public unless the node has its own credential
(`POSTVEC_API_KEY`, or `postvec login` as the service account), or the
request carries `Authorization: Bearer <api key>` on the catalogue and
pull routes. That header is used for the request and is not stored.
The dashboard **Use key** box sends it for the current tab.

The package and the image give the service account write access to
`/opt/postvec/models`. [Dashboard](/docs/server/dashboard) is the
walkthrough.

## Limits

One request carries at most 4096 items and 64 MiB of body. The execution
budget is `--predict-timeout-ms` (default 30 s). Neither route is rate
limited; put a proxy in front if the network is not yours.

- [Dashboard](/docs/server/dashboard)
- [Models on a node](/docs/server/models)
- [Node reference](/docs/server/reference)
- [External providers](/docs/models/providers)
