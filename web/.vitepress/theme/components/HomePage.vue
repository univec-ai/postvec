<script setup lang="ts">
import { withBase } from "vitepress";
import { SITE } from "../site";
</script>

<template>
  <main class="project-home">
    <div class="project-wrap">
      <header class="project-intro">
        <div class="title-row">
          <h1>postvec</h1>
          <span class="release">v{{ SITE.version }} · {{ SITE.releaseStage }}</span>
        </div>
        <p class="summary">
          A PostgreSQL extension that maintains embeddings, runs hybrid search,
          and converts vectors between embedding models.
        </p>
        <p class="description">
          postvec keeps a <code>pgvector</code> column synchronized with source
          text, combines semantic and full-text retrieval, and can convert
          stored vectors to another embedding model without re-embedding the
          source corpus.
        </p>
        <nav class="project-links" aria-label="Project documentation">
          <a :href="withBase('/docs/quickstart')">Quick start</a>
          <a :href="withBase('/docs/install/')">Install</a>
          <a :href="withBase('/docs/guides/')">Guides</a>
          <a :href="withBase('/docs/reference/sql')">SQL reference</a>
          <a :href="withBase('/download')">Releases</a>
        </nav>
        <ul class="project-meta" aria-label="Project compatibility">
          <li>PostgreSQL 16–18</li>
          <li>PostgreSQL License</li>
          <li>embedded or remote inference</li>
        </ul>
      </header>

      <section aria-labelledby="example-heading">
        <h2 id="example-heading">A minimal example</h2>
        <p>
          Enable a text column, let the background worker populate its vector
          column, then query it through the same SQL surface:
        </p>
        <div class="example-code vp-doc">
          <slot name="example" />
        </div>
        <p class="note">
          Writes are synchronized asynchronously. Query embedding is
          synchronous, and filters apply to both the semantic and full-text
          legs before reciprocal-rank fusion.
        </p>
      </section>

      <section aria-labelledby="capabilities-heading">
        <h2 id="capabilities-heading">Capabilities</h2>
        <dl class="capability-list">
          <div>
            <dt>Fully on-prem</dt>
            <dd>
              <a :href="withBase('/docs/concepts/modes#embedded')">Embedded mode</a>
              runs inference on the PostgreSQL host. Text and models remain on
              that host; no hosted inference service or provider API key is
              required.
            </dd>
          </div>
          <div>
            <dt>In-database embedding</dt>
            <dd>
              <a :href="withBase('/docs/guides/enable')"><code>enable()</code></a>
              maintains vectors from source text through a PostgreSQL
              background worker, without calls to a third-party embedding API.
            </dd>
          </div>
          <div>
            <dt>In-place migration</dt>
            <dd>
              <a :href="withBase('/docs/guides/migrate')"><code>migrate()</code></a>
              converts stored vectors directly into another model space using
              local conversion models. The source corpus is not re-embedded,
              so a long-lived knowledge base can change models without
              replaying its source text.
            </dd>
          </div>
          <div>
            <dt>Hybrid, filtered search</dt>
            <dd>
              <a :href="withBase('/docs/guides/search')"><code>search()</code></a>
              combines pgvector and PostgreSQL full-text ranks. Typed
              <a :href="withBase('/docs/guides/filters')">filters</a> constrain
              both retrieval legs before ranking.
            </dd>
          </div>
          <div>
            <dt>Model catalogue</dt>
            <dd>
              More than 100 conversion pairs and a suite of open-weight
              embedding models. The catalogue clients are implemented;
              <a :href="withBase('/docs/models/')">registry publication</a>
              is currently in preview.
            </dd>
          </div>
          <div>
            <dt>No-migration adoption</dt>
            <dd>
              <a :href="withBase('/docs/guides/bridge')">Bridge search</a>
              can query an existing corpus—including classic ada-002
              vectors—by locally embedding and converting only the new query.
            </dd>
          </div>
        </dl>
      </section>

      <section aria-labelledby="install-heading">
        <h2 id="install-heading">Installation and documentation</h2>
        <p>
          Release {{ SITE.version }} is currently marked
          <strong>{{ SITE.releaseStage }}</strong>. The release page reports
          publication status rather than presenting preview artifact names as
          live downloads.
        </p>
        <div class="doc-index">
          <div>
            <h3>Install</h3>
            <ul>
              <li><a :href="withBase('/docs/install/docker')">Docker images</a></li>
              <li><a :href="withBase('/docs/install/packages')">apt and dnf packages</a></li>
              <li><a :href="withBase('/docs/install/source')">Build from source</a></li>
              <li><a :href="withBase('/docs/install/uninstall')">Uninstall and cleanup</a></li>
            </ul>
          </div>
          <div>
            <h3>Use</h3>
            <ul>
              <li><a :href="withBase('/docs/guides/search')">Hybrid search</a></li>
              <li><a :href="withBase('/docs/guides/filters')">Typed filters</a></li>
              <li><a :href="withBase('/docs/guides/chunking')">Document chunking</a></li>
              <li><a :href="withBase('/docs/models/')">Model management</a></li>
            </ul>
          </div>
          <div>
            <h3>Operate</h3>
            <ul>
              <li><a :href="withBase('/docs/troubleshooting')">Troubleshooting</a></li>
              <li><a :href="withBase('/docs/security')">Security model</a></li>
              <li><a :href="withBase('/docs/reference/sql')">SQL reference</a></li>
              <li><a :href="withBase('/docs/reference/cli')">CLI reference</a></li>
            </ul>
          </div>
        </div>
      </section>
    </div>
  </main>
</template>

<style scoped>
.project-home {
  color: var(--vp-c-text-1);
}

.project-wrap {
  width: min(900px, calc(100% - 3rem));
  margin: 0 auto;
  padding: 4.5rem 0 5rem;
}

.project-intro {
  padding-bottom: 2rem;
  border-bottom: 1px solid var(--vp-c-divider);
}

.title-row {
  display: flex;
  flex-wrap: wrap;
  gap: 0.75rem 1rem;
  align-items: baseline;
}

h1 {
  margin: 0;
  font-size: clamp(2rem, 5vw, 2.75rem);
  font-weight: 680;
  letter-spacing: -0.045em;
  line-height: 1.1;
}

.release {
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  letter-spacing: 0.04em;
  color: var(--vp-c-text-3);
}

.summary {
  max-width: 46rem;
  margin: 0.8rem 0 0;
  font-size: 1.18rem;
  line-height: 1.55;
}

.description {
  max-width: 46rem;
  margin: 0.75rem 0 0;
  color: var(--vp-c-text-2);
  line-height: 1.65;
}

.project-links {
  display: flex;
  flex-wrap: wrap;
  gap: 0.45rem 1.2rem;
  margin-top: 1.35rem;
  font-size: 0.93rem;
}

a {
  color: var(--vp-c-brand-1);
  text-decoration: none;
}

a:hover,
a:focus-visible {
  text-decoration: underline;
  text-underline-offset: 0.2em;
}

.project-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 0.35rem 1.4rem;
  margin: 1.25rem 0 0;
  padding: 0;
  list-style: none;
  color: var(--vp-c-text-3);
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
}

.project-meta li::before {
  content: "·";
  margin-right: 0.45rem;
  color: var(--vp-c-brand-1);
}

section {
  padding: 2.25rem 0;
  border-bottom: 1px solid var(--vp-c-divider);
}

section:last-child {
  border-bottom: 0;
}

h2 {
  margin: 0 0 0.85rem;
  font-size: 1.38rem;
  font-weight: 650;
  letter-spacing: -0.025em;
  line-height: 1.3;
}

h3 {
  margin: 0 0 0.55rem;
  font-size: 1rem;
  font-weight: 650;
}

p,
li,
dd,
td {
  line-height: 1.65;
}

section > p {
  max-width: 48rem;
  margin: 0.7rem 0;
  color: var(--vp-c-text-2);
}

code {
  font-family: var(--vp-font-family-mono);
  font-size: 0.88em;
}

p code,
li code,
td code,
dt code {
  padding: 0.12rem 0.28rem;
  background: var(--vp-c-default-soft);
  border-radius: 2px;
  color: var(--vp-c-text-1);
}

.note {
  font-size: 0.9rem;
  color: var(--vp-c-text-3);
}

.capability-list {
  margin: 0;
  border-top: 1px solid var(--vp-c-divider);
}

.capability-list > div {
  display: grid;
  grid-template-columns: 11rem minmax(0, 1fr);
  gap: 1.25rem;
  padding: 0.9rem 0;
  border-bottom: 1px solid var(--vp-c-divider);
}

.capability-list dt {
  font-weight: 600;
}

.capability-list dd {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.94rem;
}

.doc-index {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 2rem;
  margin-top: 1.25rem;
}

.doc-index > div {
  padding-top: 0.8rem;
  border-top: 1px solid var(--vp-c-divider);
}

.doc-index ul {
  margin: 0;
  padding: 0;
  list-style: none;
}

.doc-index li + li {
  margin-top: 0.35rem;
}

.doc-index a {
  font-size: 0.9rem;
}

@media (max-width: 640px) {
  .project-wrap {
    width: min(100% - 2rem, 900px);
    padding-top: 3rem;
  }

  .capability-list > div {
    grid-template-columns: 1fr;
    gap: 0.3rem;
  }

  .doc-index {
    grid-template-columns: 1fr;
    gap: 1.4rem;
  }

}
</style>
