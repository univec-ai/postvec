# postvec.dev

Public documentation site for [postvec](https://postvec.dev), a PostgreSQL
extension for in-database embeddings, hybrid search and in-place vector
migration.

```bash
cd web
npm install
npm run dev      # http://localhost:5173
npm run build    # .vitepress/dist
npm run preview
```

Node 20+. Content lives in `docs/` as Markdown. The landing page,
download selector and PostgreSQL-major snippet tabs are Vue components
under `.vitepress/theme/`.

Search is VitePress local search. Release identity, GHCR image and
GitHub repo are set in `.vitepress/theme/site.ts`.
