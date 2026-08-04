---
title: Dashboard
description: Query loaded models and manage the registry from the postvec-server UI.
---

# Dashboard

A running node serves a dashboard on the discovery port (`22222`) when
the UI files are present. The `postvec-server` package and image install
them at `/opt/postvec/server/ui`. A checkout-built binary needs
`--web-ui` pointing at a built `web-ui/dist`, or the same directory
under the engine root.

Open `https://<node>:22222` (or `http://` if the node was started with
`--insecure`). The browser talks to this node and, if you choose them,
to its peers. Those URLs have to be reachable from the machine you
browse from.

The sidebar has two sections: **Query** and **Registries**. It also
lists cluster members, host memory and the models `/config` currently
advertises. The list refreshes every 30 seconds.

## Query a model

1. Select a loaded model in the sidebar.
2. For an embed model, choose **Native** or **OpenAI**. Converters use
   Native only.
3. Edit the JSON body. The header shows the `POST` route that will be
   hit.
4. Send the request (`Ctrl`+`Enter` in the editor, or the button).

The right pane shows status, latency and the raw reply.

:::: tip Expected
A valid MiniLM body `{"texts": ["hello"]}` on Native returns HTTP 200
and a 384-d vector in `data.embeddings`. OpenAI uses
`{"model": "...", "input": ["hello"]}` and returns the OpenAI list
shape. A converter takes `{"embeddings": [[...]]}`.
::::

With several nodes, **Send to** chooses which members receive the
request. Each selected peer is called from the browser; replies are
tabbed per node.

A node with nothing loaded shows "No model to query". Pull a model from
Registries, or with [`postvec model pull`](/docs/server/models), then
activate it. It appears here on the next refresh.

Contract details and curl examples: [HTTP API](/docs/server/http-api).

## Registries

This tab is `postvec model ls`, `ls --available`, `pull`, `activate`,
`deactivate` and `rm` against this node. Same on-disk result, same
receipts.

| Action | Result |
|---|---|
| Pull | Downloads, verifies and installs the model **deactivated**. Returns a job id; progress is listed under Pulls. |
| Activate | Sets `enabled: true` (including deactivated dependencies) and loads the model. |
| Deactivate | Unloads, then sets `enabled: false`. |
| Remove | Unloads and deletes a registry-installed model. |

A pull that needs a `notice` licence asks you to accept the terms before
it starts. Package-owned and manually copied models can be activated or
deactivated; they stay on disk until you remove them with the package
manager or by deleting that directory. A model another enabled model
depends on cannot be deactivated or removed.

`model upgrade` stays on the CLI.

### Reads and mutations

Listing installed models and the catalogue works on every listener.
Pull, activate, deactivate and remove on the public port need the node
started with `--manage` (`POSTVEC_SERVER_MANAGE=1`, or `"manage": true`
in the config file). Without it the tab is read-only and those actions
name `--manage` or the loopback admin port (`22223`).

`--manage` is unauthenticated, like the rest of the node. Use it on a
private network.

The package and image give the service account write access to
`/opt/postvec/models`, so a pull from the dashboard lands in the same
tree `postvec model pull` uses.

### Catalogue credential

| Source | Effect |
|---|---|
| None | Public catalogue |
| Node credential | `POSTVEC_API_KEY`, or `postvec login` run as the service account. The tab shows who the node is signed in as. |
| **Use key** box | `Authorization: Bearer` for this tab only. Forgotten when the tab closes. Never stored on the node. |

The private catalogue is a superset of the public one. A dedicated key
with a $0 spending limit is enough for registry access; hosted UniVec
inference is a separate [provider](/docs/models/providers) credential.

## Build from a checkout

```bash
cd postvec-server/web-ui
npm ci && npm run build
postvec-server --web-ui "$PWD/dist" --insecure
```

Without a built UI, `/` on the discovery port answers a JSON stub.

- [Run a node](/docs/server/node)
- [Models on a node](/docs/server/models)
- [HTTP API](/docs/server/http-api)
- [Login and private catalogue](/docs/models/login)
