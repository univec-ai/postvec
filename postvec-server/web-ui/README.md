# postvec-server web UI

Dashboard for a running `postvec-server` node. Phase 1: list the models
`GET /config` advertises and query any live node with the native
(`/api/{model}`) or OpenAI (`/api/openai/embeddings`) contract, showing the
route hit and the raw reply. Registry browse is a later release.

The mechanics match the ninference dashboard on purpose — Zustand slices,
the same `{success, data}` envelope, query cache per model — with a
separate theme. postvec is its own product.

## Develop

The Vite dev server proxies `/config` and `/api` to a local node:

```console
# terminal 1
postvec-server --root /var/lib/postvec-server --insecure

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

`dist/` is what the node serves. Point the process at it:

```console
postvec-server --web-ui ./web-ui/dist --insecure
```

or copy `dist/` to `$root/web-ui/dist`. Search order is `--web-ui` /
`POSTVEC_SERVER_WEB_UI` / `web_ui` in the config file, then
`<root>/web-ui/dist`, then next to the binary, then
`/usr/share/postvec-server/web-ui`.
