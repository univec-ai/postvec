<script setup lang="ts">
import { withBase } from "vitepress";
import { SITE } from "../site";
import PgSnippet from "./PgSnippet.vue";

/* Top of /download: the three artifact families as cards that jump to
   the matching part of the selector, and the one-command start. */
const routes = [
  {
    href: "#images",
    kicker: "PostgreSQL License",
    title: "PostgreSQL images",
    body:
      "PostgreSQL with pgvector, postvec and the CLI for majors 16, 17 and 18. The -local tags add ONNX Runtime and MiniLM; the -remote tags call postvec-server.",
    meta: SITE.ghcr,
  },
  {
    href: "#packages",
    kicker: "PostgreSQL License",
    title: "Packages",
    body:
      ".deb and .rpm for Debian 12, Ubuntu 22.04 and 24.04 and EL9, on amd64 and arm64. Install them next to an existing cluster, then run postvec setup.",
    meta: "GitHub Releases",
  },
  {
    href: "#server",
    kicker: "BUSL-1.1",
    title: "postvec-server",
    body:
      "The inference node and managed PostgreSQL worker, as one package per distribution and architecture and as an image with MiniLM loaded.",
    meta: SITE.ghcrServer,
    server: true,
  },
];
</script>

<template>
  <div class="dh">
    <p class="dh__release">
      <span class="dh__tag">{{ SITE.release }}</span>
      <span>Release {{ SITE.version }} · {{ SITE.releaseStage }}</span>
      <a :href="withBase('/docs/install/verify')">Checksums and attestations</a>
    </p>

    <div class="dh__start">
      <p class="dh__label">One container, local inference</p>
      <PgSnippet id="docker-quickstart" />
    </div>

    <div class="dh__routes">
      <a
        v-for="r in routes"
        :key="r.href"
        class="dh__route"
        :class="{ 'dh__route--server': r.server }"
        :href="r.href"
      >
        <span class="dh__kicker">{{ r.kicker }}</span>
        <span class="dh__title">{{ r.title }}</span>
        <span class="dh__body">{{ r.body }}</span>
        <code class="dh__meta">{{ r.meta }}</code>
      </a>
    </div>
    <p class="dh__server-note">
      postvec-server is source-available and free for development, testing and
      personal noncommercial use. Pro covers production use by an organization.
      <a :href="withBase('/server')">Plans and features</a>
    </p>
  </div>
</template>

<style scoped>
.dh { margin: 0.25rem 0 2.5rem; }

.dh__release {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.4rem 0.9rem;
  margin: 0 0 1.25rem !important;
  font-size: 0.88rem;
  color: var(--vp-c-text-2);
}

.dh__tag {
  padding: 0.1rem 0.45rem;
  border: 1px solid color-mix(in srgb, var(--pv-mark) 45%, var(--vp-c-border));
  border-radius: 4px;
  background: var(--vp-c-brand-soft);
  font-family: var(--vp-font-family-mono);
  font-size: 0.75rem;
  color: var(--pv-mark);
}

.dh__release a { margin-left: auto; }

.dh__start {
  padding: 1rem 1.1rem 0.25rem;
  border: 1px solid color-mix(in srgb, var(--pv-mark) 40%, var(--vp-c-border));
  border-radius: 6px;
  background:
    linear-gradient(135deg, color-mix(in srgb, var(--pv-mark) 8%, transparent), transparent 55%),
    var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-2);
}

.dh__label {
  margin: 0 !important;
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  color: var(--pv-mark);
}

.dh__start :deep(.pg-snippet) { margin: 0.6rem 0 0.8rem; }

.dh__routes {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 0.85rem;
  margin-top: 1.25rem;
}

.dh__route {
  display: flex;
  flex-direction: column;
  padding: 1rem 1.05rem;
  border: 1px solid var(--vp-c-border);
  border-radius: 6px;
  background: var(--vp-c-bg-elv);
  color: var(--vp-c-text-1) !important;
  text-decoration: none !important;
  box-shadow: var(--vp-shadow-1);
  transition: border-color 0.2s, transform 0.2s, box-shadow 0.2s;
}

.dh__route:hover {
  border-color: var(--pv-mark);
  box-shadow: var(--vp-shadow-2);
  transform: translateY(-2px);
}

.dh__route--server { border-style: dashed; }

.dh__kicker {
  font-family: var(--vp-font-family-mono);
  font-size: 0.66rem;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-3);
}

.dh__route--server .dh__kicker { color: var(--pv-mark); }

.dh__title {
  margin-top: 0.35rem;
  font-size: 1.02rem;
  font-weight: 600;
}

.dh__title::after { content: " \2193"; color: var(--vp-c-text-3); font-weight: 400; }

.dh__body {
  flex: 1;
  margin-top: 0.35rem;
  font-size: 0.86rem;
  line-height: 1.55;
  color: var(--vp-c-text-2);
}

.dh__meta {
  margin-top: 0.75rem;
  padding: 0 !important;
  background: none !important;
  font-size: 0.72rem !important;
  color: var(--vp-c-text-3) !important;
  overflow-wrap: anywhere;
}

.dh__server-note {
  margin: 0.85rem 0 0 !important;
  font-size: 0.86rem;
  color: var(--vp-c-text-3);
}

@media (max-width: 860px) {
  .dh__routes { grid-template-columns: minmax(0, 1fr); }
  .dh__release a { margin-left: 0; }
}

@media (prefers-reduced-motion: reduce) {
  .dh__route { transition: none; }
  .dh__route:hover { transform: none; }
}
</style>
