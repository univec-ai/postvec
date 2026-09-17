# postvec-server web UI

Dashboard for a running `postvec-server` node. It lists the models
`GET /config` advertises and sends queries to the local node with the native
(`/api/{model}`) or OpenAI (`/api/openai/embeddings`) contract, showing the
route and the raw reply.

The Registries tab is `postvec model ls` / `ls --available` / `pull` /
`activate` / `deactivate` against this node. Registry reads work by default.
Start the public listener with `--manage` to run those three mutations from
the dashboard. They remain available on the loopback admin listener in any
case.

State handling follows the ninference dashboard (Zustand slices, the
`{success, data}` envelope, a query cache per model) so the two front-ends
stay familiar to work on. Layout is plain CSS with system fonts
(`src/styles/app.css`): a sidebar of models and a request/response workbench.
Network traffic is the node itself.

## Develop

The Vite dev server proxies `/config` and `/api` to a local node:

```console
# terminal 1
postvec-server --insecure

# terminal 2
cd postvec-server/web-ui
npm install
npm run dev
```

Open http://127.0.0.1:3000. Override the proxy target with
`VITE_API_ENDPOINT=https://10.0.0.10:22222`.

## Build

```console
npm ci
npm run build
```

`dist/` is what the node serves. Point a checkout-built node at it:

```console
postvec-server --web-ui "$PWD/dist" --insecure
```

or install it at `<root>/server/ui`, which is where the `postvec-server`
package and image put it (`packaging/postvec/scripts/build-ui-bundle.sh`
builds it in a pinned node container for a release). Resolution order:
`--web-ui` / `POSTVEC_SERVER_WEB_UI` / `web_ui` in the config file, else
`<root>/server/ui`.
