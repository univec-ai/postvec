# postvec.dev

Public documentation site for [postvec](https://postvec.dev) — the
PostgreSQL extension that keeps embeddings in sync, searches them, and
migrates them between models in place.

```bash
cd web/postvec.dev
npm install
npm run dev      # http://localhost:5173
npm run build    # .vitepress/dist
npm run preview
```

Node 20+. Content lives in `docs/` as Markdown. The landing page and
download selector are Vue components under `.vitepress/theme/`.

Search is VitePress local search (no third-party service).

Release identity, GHCR image, and GitHub repo are set in
`.vitepress/theme/site.ts`.
