<script setup lang="ts">
import { withBase } from "vitepress";
import { SITE } from "../site";
</script>

<template>
  <main class="home-page">
    <div class="wrap">
      <header class="intro">
        <p class="kicker">
          PostgreSQL 16–18 · PostgreSQL License · {{ SITE.releaseStage }}
          {{ SITE.version }}
        </p>
        <h1>Embeddings that live in the table.<br />Models that do not have to.</h1>
        <p class="lede">
          postvec is a PostgreSQL extension. It keeps a
          <code>pgvector</code> column synchronized with source text, runs
          hybrid full-text and semantic search in one call, and converts stored
          vectors from one embedding model to another without sending the
          corpus through an embedding API again.
        </p>
        <p class="follow">
          Inference runs on the database host in
          <a :href="withBase('/docs/concepts/modes')">embedded mode</a>,
          or on a ninference fleet in remote mode. There is no third-party
          embedding key in PostgreSQL.
        </p>
        <nav class="links" aria-label="Primary documentation">
          <a :href="withBase('/docs/quickstart')">Quick start</a>
          <a :href="withBase('/docs/install/')">Install</a>
          <a :href="withBase('/docs/guides/')">Usage</a>
          <a :href="withBase('/download')">Downloads</a>
        </nav>
      </header>

      <section aria-labelledby="path-heading">
        <h2 id="path-heading">A working path</h2>
        <ol class="path">
          <li>
            <a :href="withBase('/docs/quickstart')">Run the embedded image</a>
            if the host cluster should stay untouched.
          </li>
          <li>
            <a :href="withBase('/docs/guides/enable')"><code>enable()</code></a>
            a text column, or
            <a :href="withBase('/docs/guides/adopt')"><code>adopt()</code></a>
            an existing vector column.
          </li>
          <li>
            Wait until <code>pending_jobs = 0</code>, then
            <a :href="withBase('/docs/guides/search')"><code>search()</code></a>.
          </li>
          <li>
            Later,
            <a :href="withBase('/docs/guides/migrate')"><code>migrate()</code></a>
            the stored space, or
            <a :href="withBase('/docs/guides/bridge')">bridge each query</a>
            and leave the column as it is.
          </li>
        </ol>
      </section>

      <section aria-labelledby="example-heading">
        <h2 id="example-heading">The SQL surface</h2>
        <p>
          Application work is SQL. The CLI configures the cluster and, in
          embedded mode, the models.
        </p>
        <div class="example-code vp-doc">
          <slot name="example" />
        </div>
        <p class="note">
          Writes fill asynchronously — normally within inference time of
          commit. Query embedding is synchronous. Filters apply to both legs
          before reciprocal-rank fusion.
        </p>
      </section>

      <section aria-labelledby="ideas-heading">
        <h2 id="ideas-heading">Vector lock-in and embedding debt</h2>
        <p>
          An embedding is a point in the space of the model that produced it.
          Matching dimensions do not make two models comparable. Once a corpus,
          its index, and every query caller assume space A, the model is part
          of the data contract. That coupling is
          <strong>vector lock-in</strong>.
        </p>
        <p>
          Changing the contract normally means re-embedding the corpus,
          rebuilding the index, dual-writing through cutover, and re-evaluating
          retrieval. Every new row in the old space increases that deferred
          work. The accumulated cost is
          <strong>embedding debt</strong>.
        </p>
        <p>
          postvec names both so they can be operated on.
          <a :href="withBase('/docs/concepts/lock-in')">The definitions</a>
          sit next to the two available operations: convert the stored vectors,
          or convert only the query.
        </p>
      </section>

      <section aria-labelledby="capabilities-heading">
        <h2 id="capabilities-heading">What the extension does</h2>
        <dl class="caps">
          <div>
            <dt>On the database host</dt>
            <dd>
              Embedded mode runs inference inside the PostgreSQL launcher.
              Text and model weights stay on that host. No hosted embedding
              API is required.
            </dd>
          </div>
          <div>
            <dt>In the table</dt>
            <dd>
              <code>enable()</code> maintains vectors from source text through
              a background worker. Application SQL never calls an embedding
              provider.
            </dd>
          </div>
          <div>
            <dt>In place</dt>
            <dd>
              <code>migrate()</code> translates stored vectors into another
              model space with local conversion models. The source corpus is
              not replayed. A knowledge base can change models and keep its
              rows.
            </dd>
          </div>
          <div>
            <dt>Hybrid, filtered</dt>
            <dd>
              <code>search()</code> fuses pgvector and PostgreSQL full-text
              ranks. Typed filters constrain both legs before ranking.
            </dd>
          </div>
          <div>
            <dt>Without moving the corpus</dt>
            <dd>
              Bridge search embeds a new query locally and converts that one
              vector into an existing space — including classic ada-002
              columns — so the stored bytes can stay put.
            </dd>
          </div>
          <div>
            <dt>With a local catalogue</dt>
            <dd>
              Open-weight embedders and conversion pairs are pulled onto the
              host. The public channel is a subset; a verified UniVec account
              sees the private superset ({{ SITE.conversionPairs }} conversion
              pairs, a broader embed suite). Registry publication is
              {{ SITE.registryStage }}.
            </dd>
          </div>
        </dl>
      </section>

      <section aria-labelledby="modes-heading">
        <h2 id="modes-heading">Two inference modes, one SQL</h2>
        <div class="modes">
          <article>
            <h3>Embedded</h3>
            <p>
              The engine lives in the launcher. Use this when text must not
              leave the database host, or when there is no inference fleet.
              Models are administered with <code>postvec model …</code>.
            </p>
          </article>
          <article>
            <h3>Remote</h3>
            <p>
              Backends call ninference over gRPC. Use this for GPU or
              distributed inference, and for organisation deployments that
              serve the private catalogue from that fleet. The SQL does not
              change.
            </p>
          </article>
        </div>
      </section>

      <section aria-labelledby="docs-heading">
        <h2 id="docs-heading">Documentation</h2>
        <div class="index">
          <div>
            <h3>Install</h3>
            <ul>
              <li><a :href="withBase('/docs/install/docker')">Docker</a></li>
              <li><a :href="withBase('/docs/install/packages')">apt and dnf</a></li>
              <li><a :href="withBase('/docs/install/source')">Source</a></li>
              <li><a :href="withBase('/docs/install/uninstall')">Uninstall</a></li>
            </ul>
          </div>
          <div>
            <h3>Use</h3>
            <ul>
              <li><a :href="withBase('/docs/guides/search')">Search</a></li>
              <li><a :href="withBase('/docs/guides/chunking')">Chunking</a></li>
              <li><a :href="withBase('/docs/guides/migrate')">Migrate</a></li>
              <li><a :href="withBase('/docs/models/')">Models</a></li>
            </ul>
          </div>
          <div>
            <h3>Operate</h3>
            <ul>
              <li><a :href="withBase('/docs/troubleshooting')">Troubleshooting</a></li>
              <li><a :href="withBase('/docs/security')">Security</a></li>
              <li><a :href="withBase('/docs/reference/sql')">SQL reference</a></li>
              <li><a :href="withBase('/docs/reference/cli')">CLI reference</a></li>
            </ul>
          </div>
        </div>
        <p class="note">
          Release {{ SITE.version }} is {{ SITE.releaseStage }}. The
          <a :href="withBase('/download')">downloads page</a> reports whether
          the named artifacts exist on GitHub; until they do, the names are
          the local-build contract.
        </p>
      </section>
    </div>
  </main>
</template>

<style scoped>
.home-page {
  color: var(--vp-c-text-1);
}

.wrap {
  width: min(44rem, calc(100% - 3rem));
  margin: 0 auto;
  padding: 4.25rem 0 5rem;
}

.kicker {
  margin: 0 0 1.1rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  letter-spacing: 0.04em;
  color: var(--vp-c-text-3);
}

h1 {
  margin: 0;
  font-family: var(--pv-font-display);
  font-size: clamp(2rem, 4.6vw, 2.85rem);
  font-weight: 500;
  letter-spacing: -0.03em;
  line-height: 1.12;
}

h1::after {
  content: "";
  display: block;
  width: 2.4rem;
  height: 2px;
  margin-top: 1.15rem;
  background: var(--pv-mark);
}

.lede,
.follow {
  max-width: 40rem;
  margin: 1.15rem 0 0;
  font-size: 1.05rem;
  line-height: 1.65;
}

.follow {
  color: var(--vp-c-text-2);
  font-size: 1rem;
}

.links {
  display: flex;
  flex-wrap: wrap;
  gap: 0.4rem 1.35rem;
  margin-top: 1.6rem;
  font-size: 0.95rem;
}

a {
  color: var(--vp-c-brand-1);
  text-decoration: none;
}

a:hover,
a:focus-visible {
  text-decoration: underline;
  text-underline-offset: 0.18em;
}

section {
  padding: 2.35rem 0;
  border-top: 1px solid var(--vp-c-divider);
}

.intro {
  padding-bottom: 2.4rem;
}

h2 {
  margin: 0 0 0.85rem;
  font-family: var(--pv-font-display);
  font-size: 1.35rem;
  font-weight: 500;
  letter-spacing: -0.02em;
}

h3 {
  margin: 0 0 0.45rem;
  font-family: var(--pv-font-display);
  font-size: 1.02rem;
  font-weight: 500;
}

section > p {
  margin: 0.65rem 0;
  color: var(--vp-c-text-2);
  line-height: 1.68;
}

code {
  font-family: var(--vp-font-family-mono);
  font-size: 0.88em;
}

p code,
li code,
dd code {
  padding: 0.08rem 0.26rem;
  background: var(--vp-c-default-soft);
  color: var(--vp-c-text-1);
}

.note {
  font-size: 0.9rem;
  color: var(--vp-c-text-3);
}

.path {
  margin: 0.4rem 0 0;
  padding: 0;
  list-style: none;
  counter-reset: step;
}

.path li {
  position: relative;
  padding: 0.55rem 0 0.55rem 2.1rem;
  border-top: 1px solid var(--vp-c-divider);
  color: var(--vp-c-text-2);
  line-height: 1.55;
}

.path li:last-child {
  border-bottom: 1px solid var(--vp-c-divider);
}

.path li::before {
  counter-increment: step;
  content: counter(step);
  position: absolute;
  left: 0;
  top: 0.6rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  color: var(--pv-mark);
}

.caps {
  margin: 0.4rem 0 0;
}

.caps > div {
  display: grid;
  grid-template-columns: 11.5rem minmax(0, 1fr);
  gap: 1.1rem;
  padding: 0.85rem 0;
  border-top: 1px solid var(--vp-c-divider);
}

.caps > div:last-child {
  border-bottom: 1px solid var(--vp-c-divider);
}

.caps dt {
  font-weight: 600;
}

.caps dd {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
  line-height: 1.6;
}

.modes {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 1.75rem;
  margin-top: 0.6rem;
}

.modes p {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
  line-height: 1.6;
}

.index {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 1.75rem;
  margin: 0.85rem 0 1.1rem;
}

.index ul {
  margin: 0;
  padding: 0;
  list-style: none;
}

.index li + li {
  margin-top: 0.32rem;
}

.index a {
  font-size: 0.92rem;
}

@media (max-width: 720px) {
  .wrap {
    width: min(100% - 2rem, 44rem);
    padding-top: 3rem;
  }

  .caps > div,
  .modes,
  .index {
    grid-template-columns: 1fr;
    gap: 0.85rem;
  }
}
</style>
