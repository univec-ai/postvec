<script setup lang="ts">
import { withBase } from "vitepress";
import { SITE } from "../site";
import PgSnippet from "./PgSnippet.vue";
</script>

<template>
  <main class="home-page">
    <div class="wrap">
      <header class="intro">
        <p class="kicker">
          PostgreSQL 16, 17 and 18 · PostgreSQL License ·
          {{ SITE.releaseStage }} {{ SITE.version }}
        </p>
        <h1>Embeddings that live in the table.<br />Models that do not have to.</h1>
        <p class="lede">
          A knowledge base stored as vectors is tied to the embedding model
          that produced them. Providers retire a generation every year or
          two. Search then only works after the corpus is re-embedded and
          the index rebuilt. The next generation repeats the same work.
        </p>
        <dl class="defs">
          <div>
            <dt>Vector lock-in</dt>
            <dd>
              The dependency between stored vectors and the model that
              produced them.
            </dd>
          </div>
          <div>
            <dt>Embedding debt</dt>
            <dd>The cost of changing that dependency later.</dd>
          </div>
        </dl>
        <p class="follow">
          postvec operates on both. It keeps a
          <code>pgvector</code> column in step with source text, runs hybrid
          full-text and semantic search in one call, and can convert stored
          vectors from one model space to another.
        </p>
        <nav class="links" aria-label="Primary documentation">
          <a :href="withBase('/docs/quickstart')">Quick start</a>
          <a :href="withBase('/docs/install/')">Install</a>
          <a :href="withBase('/docs/guides/starting')">Choose the SQL call</a>
          <a :href="withBase('/download')">Downloads</a>
        </nav>
      </header>

      <section aria-labelledby="snapshot-heading">
        <h2 id="snapshot-heading">What it does</h2>
        <dl class="caps">
          <div>
            <dt>Local models</dt>
            <dd>
              Open-weight models run inside PostgreSQL or on on-prem
              inference nodes.
            </dd>
          </div>
          <div>
            <dt>Evergreen knowledge bases</dt>
            <dd>
              Existing vectors convert between embedding spaces through a
              catalogue of {{ SITE.conversionPairs }} UniVec pairs.
              Conversion uses the stored vectors.
            </dd>
          </div>
          <div>
            <dt>Search a retired space</dt>
            <dd>
              An ada-002 column can stay as it is. postvec embeds the query
              with an available model and converts that one vector into the
              stored space.
            </dd>
          </div>
        </dl>
      </section>

      <section aria-labelledby="start-heading">
        <h2 id="start-heading">Try it on this machine</h2>
        <p>
          A disposable container is enough for a first look. Tabs pick
          the PostgreSQL major; the same choice is remembered on the
          install pages.
        </p>
        <PgSnippet
          id="docker-quickstart"
          caption="Wait until the container is healthy, then follow the quick start. Persistent volumes and package installs are on the install pages. RDS and Aurora cannot load the worker."
        />
        <p class="note">
          Images live on GHCR. Packages are GitHub Release assets. See
          <a :href="withBase('/download')">downloads</a> for names and
          publication status.
        </p>
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
          Writes fill in the background, usually within inference time of
          commit. Query embedding is synchronous. Filters apply to both
          legs before reciprocal-rank fusion.
        </p>
      </section>

      <section aria-labelledby="which-heading">
        <h2 id="which-heading">Choose the SQL call</h2>
        <ol class="path">
          <li>
            Text, no vectors:
            <a :href="withBase('/docs/guides/enable')"><code>enable()</code></a>
            creates and maintains a shadow column.
          </li>
          <li>
            A populated vector column:
            <a :href="withBase('/docs/guides/adopt')"><code>adopt()</code></a>
            registers it without rewriting the bytes.
          </li>
          <li>
            A retired or provider-only space:
            <a :href="withBase('/docs/guides/bridge')">bridge search</a>
            converts each query into that space.
          </li>
          <li>
            Ready for a new model:
            <a :href="withBase('/docs/guides/migrate')"><code>migrate()</code></a>
            converts the stored vectors in place.
          </li>
        </ol>
      </section>

      <section aria-labelledby="modes-heading">
        <h2 id="modes-heading">Two inference modes, one SQL</h2>
        <div class="modes">
          <article>
            <h3>Embedded</h3>
            <p>
              The engine lives in the PostgreSQL launcher. Text and
              weights stay on the database host. The extension, CLI and
              packages are under the PostgreSQL License.
            </p>
          </article>
          <article>
            <h3>Remote</h3>
            <p>
              Backends call ninference over gRPC for distributed CPU or
              GPU inference. That server is licensed separately, under
              community (non-commercial) and organisation terms. The SQL
              stays the same.
            </p>
          </article>
        </div>
        <p class="note">
          Converter weights are a UniVec product. The public catalogue is
          a subset; a verified account sees the private superset. The
          bundled MiniLM model works offline.
        </p>
      </section>

      <section aria-labelledby="docs-heading">
        <h2 id="docs-heading">Documentation</h2>
        <div class="index">
          <div>
            <h3>Install</h3>
            <ul>
              <li><a :href="withBase('/docs/quickstart')">Quick start</a></li>
              <li><a :href="withBase('/docs/install/docker')">Docker</a></li>
              <li><a :href="withBase('/docs/install/packages')">Packages</a></li>
              <li><a :href="withBase('/docs/install/uninstall')">Uninstall</a></li>
            </ul>
          </div>
          <div>
            <h3>Use</h3>
            <ul>
              <li><a :href="withBase('/docs/guides/starting')">Choose the SQL call</a></li>
              <li><a :href="withBase('/docs/guides/search')">Search</a></li>
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
          the named artifacts exist on GitHub. Until they do, the names are
          the local-build contract. Search in the header jumps to a function
          or topic.
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
  width: min(48rem, calc(100% - 3rem));
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
  max-width: 42rem;
  margin: 1.15rem 0 0;
  font-size: 1.05rem;
  line-height: 1.65;
}

.follow {
  color: var(--vp-c-text-2);
  font-size: 1rem;
}

.defs {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 0;
  margin: 1.5rem 0 0;
  border-top: 1px solid var(--vp-c-divider);
  border-bottom: 1px solid var(--vp-c-divider);
}

.defs > div {
  padding: 0.95rem 1.15rem 1rem 0;
}

.defs > div + div {
  padding-left: 1.15rem;
  padding-right: 0;
  border-left: 1px solid var(--vp-c-divider);
}

.defs dt {
  font-family: var(--pv-font-display);
  font-weight: 500;
  margin-bottom: 0.3rem;
}

.defs dd {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
  line-height: 1.55;
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
  grid-template-columns: 14.5rem minmax(0, 1fr);
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
    width: min(100% - 2rem, 48rem);
    padding-top: 3rem;
  }

  .defs,
  .caps > div,
  .modes,
  .index {
    grid-template-columns: 1fr;
    gap: 0.85rem;
  }

  .defs > div,
  .defs > div + div {
    padding: 0.85rem 0;
    border-left: 0;
  }

  .defs > div + div {
    border-top: 1px solid var(--vp-c-divider);
  }
}
</style>
