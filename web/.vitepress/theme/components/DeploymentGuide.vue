<script setup lang="ts">
import { ref } from "vue";
import { withBase } from "vitepress";

const selected = ref(0);
const deployments = [
  {
    name: "Embedded", title: "Run models on the database host",
    body: "The extension keeps vectors in sync and runs local inference inside PostgreSQL. The bundled MiniLM model provides a starting point for semantic search.",
    database: "PostgreSQL + postvec", worker: "Sync worker + inference", connection: "Inside PostgreSQL",
    detail: "Install the extension on a host you administer. Setup requires shared_preload_libraries and a PostgreSQL restart.",
    license: "Extension: PostgreSQL License, for any use.", href: "/docs/quickstart", link: "Embedded quick start",
  },
  {
    name: "Remote inference", title: "Give inference its own resources",
    body: "The extension runs the sync worker in PostgreSQL and sends inference requests to postvec-server. Run the server alongside the database or on separate CPU or GPU hosts.",
    database: "PostgreSQL + postvec", worker: "postvec-server", connection: "Inference over gRPC",
    detail: "Use the extension's SQL functions with a separately managed inference process. GPU execution requires a compatible server build and runtime.",
    license: "Server: source-available; Pro covers organizational production use.", href: "/docs/quickstart-remote", link: "Remote quick start",
  },
  {
    name: "Managed PostgreSQL", title: "Connect the server to your database",
    body: "postvec-server installs a SQL schema and runs the sync worker outside PostgreSQL. Stored vectors and search indexes remain in your managed database.",
    database: "PostgreSQL + pgvector", worker: "postvec-server + sync worker", connection: "Database connection over SQL",
    detail: "Requires pgvector 0.8+, a direct database endpoint and table-owner permissions for the worker. SQL text search uses the server's query proxy.",
    license: "Server: source-available; one 30-day production evaluation per organization.", href: "/docs/server/managed", link: "Managed PostgreSQL guide",
  },
];
</script>

<template>
  <div class="deployment">
    <div class="deployment__choices" role="group" aria-label="Deployment options">
      <button v-for="(item, index) in deployments" :key="item.name" type="button"
        :aria-pressed="selected === index" @click="selected = index">
        <span class="deployment__number">0{{ index + 1 }}</span>{{ item.name }}
      </button>
    </div>
    <div class="deployment__content" aria-live="polite" aria-atomic="true">
      <div>
        <h3>{{ deployments[selected].title }}</h3>
        <p>{{ deployments[selected].body }}</p>
        <p class="deployment__detail">{{ deployments[selected].detail }}</p>
        <a :href="withBase(deployments[selected].href)">{{ deployments[selected].link }} <span aria-hidden="true">&rarr;</span></a>
      </div>
      <div class="deployment__diagram" :class="{ 'deployment__diagram--embedded': selected === 0 }">
        <span class="deployment__node">{{ deployments[selected].database }}</span>
        <span class="deployment__connection">{{ deployments[selected].connection }}</span>
        <span class="deployment__node deployment__node--inference">{{ deployments[selected].worker }}</span>
        <p>{{ deployments[selected].license }}</p>
      </div>
    </div>
  </div>
</template>

<style scoped>
.deployment { border: 1px solid var(--vp-c-border); border-radius: 10px; overflow: hidden; background: var(--vp-c-bg-elv); }
.deployment__choices { display: grid; grid-template-columns: repeat(3, 1fr); border-bottom: 1px solid var(--vp-c-border); }
.deployment__choices button { padding: 1.1rem; text-align: left; font: inherit; color: var(--vp-c-text-2); border-bottom: 3px solid transparent; }
.deployment__choices button[aria-pressed="true"] { color: var(--vp-c-text-1); background: var(--vp-c-brand-soft); border-bottom-color: var(--pv-mark); }
.deployment__choices button:focus-visible { outline-offset: -4px; }
.deployment__number { margin-right: 0.6rem; font-family: var(--vp-font-family-mono); font-size: 0.75rem; color: var(--pv-mark); }
.deployment__content { display: grid; grid-template-columns: 1.2fr 1fr; gap: 3rem; padding: 2rem; align-items: center; }
.deployment h3 { margin: 0 0 0.8rem; font-size: 1.3rem; font-weight: 500; }
.deployment p { margin: 0 0 1rem; line-height: 1.65; color: var(--vp-c-text-2); }
.deployment .deployment__detail { font-size: 0.9rem; }
.deployment a { color: var(--vp-c-brand-1); font-weight: 500; }
.deployment a:hover { text-decoration: underline; }
.deployment__diagram { padding: 1.5rem; border: 1px dashed var(--vp-c-border); border-radius: 8px; background: radial-gradient(ellipse at top, var(--vp-c-brand-soft), transparent 75%); text-align: center; }
.deployment__node { display: block; padding: 0.8rem; border: 1px solid var(--vp-c-border); border-radius: 5px; background: var(--vp-c-bg); font-family: var(--vp-font-family-mono); font-size: 0.85rem; }
.deployment__node--inference { border-color: var(--pv-mark); }
.deployment__connection { display: block; padding: 1rem 0; font-size: 0.78rem; color: var(--vp-c-text-2); }
.deployment__diagram--embedded { border-style: solid; border-color: var(--pv-mark); }
.deployment__diagram p { margin: 1rem 0 0; font-size: 0.78rem; }
@media (max-width: 720px) {
  .deployment__content { grid-template-columns: 1fr; gap: 1.5rem; padding: 1.25rem; }
  .deployment__choices button { padding: 0.85rem 0.6rem; font-size: 0.8rem; }
  .deployment__number { display: block; margin-bottom: 0.25rem; }
}
</style>
