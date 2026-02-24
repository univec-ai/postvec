<script setup lang="ts">
import { ref } from "vue";
import { withBase } from "vitepress";
import { SITE } from "../site";
import PgSnippet from "./PgSnippet.vue";

type Tab = {
  id: string;
  label: string;
  state: string;
  note: string;
  href: string;
  linkText: string;
};

const tabs: Tab[] = [
  {
    id: "enable",
    label: "ENABLE ON A TEXT COLUMN",
    state: "A text column that should become searchable.",
    note:
      "postvec adds a vector column beside the source, installs the triggers that keep it current, and fills it in the background.",
    href: "/docs/guides/enable",
    linkText: "Enable a column",
  },
  {
    id: "search",
    label: "HYBRID SEARCH",
    state: "One call, ranked rows.",
    note:
      "Semantic and full-text candidates get filtered, ranked and fused. The primary key comes back as text, so the join works on any table shape.",
    href: "/docs/guides/search",
    linkText: "Search and filters",
  },
  {
    id: "adopt",
    label: "ENABLE ON PRE-EXISTING VECTORS",
    state: "A populated pgvector column, filled by application code.",
    note:
      "adopt() registers the column and leaves every stored byte alone. If the model behind it is retired, queries are embedded with an available model and converted into that space.",
    href: "/docs/guides/adopt",
    linkText: "Adopt existing vectors",
  },
  {
    id: "migrate",
    label: "MIGRATE VECTOR FORMATS",
    state: "The stored space is the one that has to move.",
    note:
      "Stored vectors convert in batches into a new column while writes keep flowing. The columns swap at finalization. The source text stays put.",
    href: "/docs/guides/migrate",
    linkText: "Migrate in place",
  },
];

const active = ref(tabs[0].id);
</script>

<template>
  <main class="home-page">
    <div class="wrap">
      <header class="hero">
        <div class="hero__copy">
          <h1>
            Managed embeddings, hybrid search and vector conversion
            for PostgreSQL
          </h1>
          <p class="subhead">
            In-database inference using local models or remote via 3rd
            party providers. Compatible with Pg 16, 17, 18.
          </p>
          <p class="lede">
            postvec makes a text column semantic. It keeps a
            <code>pgvector</code> column beside your data, answers
            full-text and semantic queries in one call, and converts
            stored vectors from one model space into another.
          </p>
          <p class="lede lede--soft">
            That last part is why it exists. A knowledge base held as
            vectors is tied to the model that produced them. Providers
            retire a generation every year or two. The usual answer is
            to re-embed the corpus and rebuild the index. Then you do
            it again at the next generation.
          </p>
          <nav class="links" aria-label="Primary documentation">
            <a class="lead-link" :href="withBase('/docs/quickstart')">Quick start</a>
            <a :href="withBase('/docs/install/')">Install</a>
            <a :href="withBase('/docs/guides/starting')">Which SQL call</a>
            <a :href="withBase('/download')">Downloads</a>
          </nav>
        </div>

        <aside class="hero__aside" aria-label="Definitions and a first container">
          <dl class="defs">
            <div>
              <dt>Vector lock-in</dt>
              <dd>
                The dependency between stored vectors and the model
                that produced them.
              </dd>
            </div>
            <div>
              <dt>Embedding debt</dt>
              <dd>The cost of changing that dependency later.</dd>
            </div>
          </dl>
          <p class="follow">
            postvec is how those two are operated on.
          </p>
          <div class="hero__try">
            <p class="hero__try-label">A disposable container</p>
            <PgSnippet
              id="docker-quickstart"
              caption="PostgreSQL, pgvector, postvec and MiniLM. Tabs pick the major."
            />
            <p class="note">
              Then the
              <a :href="withBase('/docs/quickstart')">quick start</a>.
              Packages and source builds are on
              <a :href="withBase('/docs/install/')">Install</a>.
              RDS and Aurora cannot load the worker.
            </p>
          </div>
        </aside>
      </header>

      <section class="band band--facts" aria-label="What it does">
        <dl class="caps">
          <div>
            <dt>No keys in your database</dt>
            <dd>
              Embedding runs on open-weight models by default, either
              inside PostgreSQL or on inference nodes you operate, and
              the text stays on the host. Hosted providers are opt-in
              per column; their credentials live in the inference
              layer, never in the database.
              <a :href="withBase('/docs/models/providers')">External providers</a>.
            </dd>
          </div>
          <div>
            <dt>Knowledge bases that outlast a model</dt>
            <dd>
              Stored vectors convert between embedding spaces through a
              catalogue of {{ SITE.conversionPairs }} UniVec pairs.
              Conversion reads the vectors.
              <a :href="withBase('/docs/guides/migrate')">Migrate in place</a>.
            </dd>
          </div>
          <div>
            <dt>Retired spaces stay searchable</dt>
            <dd>
              A column of <code>ada-002</code> vectors can stay exactly
              as it is. postvec embeds the query with a model that is
              still around and converts that one vector into the stored
              space.
              <a :href="withBase('/docs/guides/bridge')">Bridge search</a>.
            </dd>
          </div>
        </dl>
      </section>

      <section class="band" aria-labelledby="figure-heading">
        <div class="band__head band__head--row">
          <h2 id="figure-heading">How a request moves</h2>
          <p>
            Writes go through a background worker. Search embeds the
            query on the spot. Changing model works on the stored
            vectors.
          </p>
        </div>

        <figure class="pvd">
          <svg viewBox="0 0 1040 480" role="img" aria-labelledby="pvd-home-title pvd-home-desc">
          <title id="pvd-home-title">The three paths through postvec</title>
          <desc id="pvd-home-desc">Three panels. On the write path a committed row is enqueued by a trigger into postvec.jobs, embedded by the worker and written back to the vector column. On the read path one call embeds the query, ranks a semantic and a full-text leg and fuses the two rankings into ranked primary keys. On the model-change path stored vectors pass through the UniVec converter into a new model space while the source text stays put.</desc>
          <defs>
            <marker id="pvd-hm-head" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
              <polygon class="head" points="0 0, 8 3, 0 6"/>
            </marker>
          </defs>

          <rect class="s-mask" width="1040" height="480"/>

          <rect class="s-zone" x="16" y="40" width="320" height="408" rx="8"/>
          <rect class="s-zone" x="360" y="40" width="320" height="408" rx="8"/>
          <rect class="s-zone" x="704" y="40" width="320" height="408" rx="8"/>

          <path class="c" d="M 176,128 V 168" marker-end="url(#pvd-hm-head)"/>
          <path class="c" d="M 176,224 V 264" marker-end="url(#pvd-hm-head)"/>
          <path class="c" d="M 176,320 V 360" marker-end="url(#pvd-hm-head)"/>

          <path class="c" d="M 456,128 V 168" marker-end="url(#pvd-hm-head)"/>
          <path class="c" d="M 584,128 V 168" marker-end="url(#pvd-hm-head)"/>
          <path class="c" d="M 456,224 V 264" marker-end="url(#pvd-hm-head)"/>
          <path class="c" d="M 584,224 V 264" marker-end="url(#pvd-hm-head)"/>
          <path class="c" d="M 520,320 V 360" marker-end="url(#pvd-hm-head)"/>

          <path class="c" d="M 864,128 V 216" marker-end="url(#pvd-hm-head)"/>
          <path class="c" d="M 864,272 V 360" marker-end="url(#pvd-hm-head)"/>

          <rect class="s-mask" x="48" y="34" width="64" height="12"/>
          <text class="t-zone" x="80" y="43" text-anchor="middle">WRITE PATH</text>
          <rect class="s-mask" x="392" y="34" width="60" height="12"/>
          <text class="t-zone" x="422" y="43" text-anchor="middle">READ PATH</text>
          <rect class="s-mask" x="736" y="34" width="76" height="12"/>
          <text class="t-zone" x="774" y="43" text-anchor="middle">MODEL CHANGE</text>

          <rect class="s-mask" x="184" y="142" width="48" height="12"/>
          <text class="t-arrow" x="208" y="151" text-anchor="middle">TRIGGER</text>
          <rect class="s-mask" x="184" y="238" width="48" height="12"/>
          <text class="t-arrow" x="208" y="247" text-anchor="middle">WORKER</text>
          <rect class="s-mask" x="184" y="334" width="64" height="12"/>
          <text class="t-arrow" x="216" y="343" text-anchor="middle">WRITE-BACK</text>

          <rect class="s-mask" x="464" y="142" width="40" height="12"/>
          <text class="t-arrow" x="484" y="151" text-anchor="middle">EMBED</text>

          <rect class="s-mask" x="872" y="166" width="56" height="12"/>
          <text class="t-arrow" x="900" y="175" text-anchor="middle">migrate()</text>
          <rect class="s-mask" x="872" y="310" width="92" height="12"/>
          <text class="t-arrow" x="918" y="319" text-anchor="middle">FINALIZE · SWAP</text>

          <rect class="s-mask" x="56" y="72" width="240" height="56" rx="6"/>
          <rect class="s-store" x="56" y="72" width="240" height="56" rx="6"/>
          <text class="t-name" x="176" y="96" text-anchor="middle">docs.body</text>
          <text class="t-sub" x="176" y="110" text-anchor="middle">source text</text>

          <rect class="s-mask" x="56" y="168" width="240" height="56" rx="6"/>
          <rect class="s-store" x="56" y="168" width="240" height="56" rx="6"/>
          <text class="t-name" x="176" y="192" text-anchor="middle">postvec.jobs</text>
          <text class="t-sub" x="176" y="206" text-anchor="middle">coalesced per row</text>

          <rect class="s-mask" x="56" y="264" width="240" height="56" rx="6"/>
          <rect class="s-node" x="56" y="264" width="240" height="56" rx="6"/>
          <text class="t-name" x="176" y="288" text-anchor="middle">Embedding model</text>
          <text class="t-sub" x="176" y="302" text-anchor="middle">embedded or remote</text>

          <rect class="s-mask" x="56" y="360" width="240" height="56" rx="6"/>
          <rect class="s-store" x="56" y="360" width="240" height="56" rx="6"/>
          <text class="t-name" x="176" y="384" text-anchor="middle">docs.body_semantic</text>
          <text class="t-sub" x="176" y="398" text-anchor="middle">vector(384)</text>

          <rect class="s-mask" x="400" y="72" width="240" height="56" rx="6"/>
          <rect class="s-node" x="400" y="72" width="240" height="56" rx="6"/>
          <text class="t-name" x="520" y="96" text-anchor="middle">Query text</text>
          <text class="t-sub" x="520" y="110" text-anchor="middle">one search() call</text>

          <rect class="s-mask" x="400" y="168" width="112" height="56" rx="6"/>
          <rect class="s-node" x="400" y="168" width="112" height="56" rx="6"/>
          <text class="t-name" x="456" y="192" text-anchor="middle">Semantic</text>
          <text class="t-sub" x="456" y="206" text-anchor="middle">pgvector rank</text>

          <rect class="s-mask" x="528" y="168" width="112" height="56" rx="6"/>
          <rect class="s-node" x="528" y="168" width="112" height="56" rx="6"/>
          <text class="t-name" x="584" y="192" text-anchor="middle">Full-text</text>
          <text class="t-sub" x="584" y="206" text-anchor="middle">tsvector rank</text>

          <rect class="s-mask" x="400" y="264" width="240" height="56" rx="6"/>
          <rect class="s-node" x="400" y="264" width="240" height="56" rx="6"/>
          <text class="t-name" x="520" y="288" text-anchor="middle">Fusion</text>
          <text class="t-sub" x="520" y="302" text-anchor="middle">reciprocal rank</text>

          <rect class="s-mask" x="400" y="360" width="240" height="56" rx="6"/>
          <rect class="s-node" x="400" y="360" width="240" height="56" rx="6"/>
          <text class="t-name" x="520" y="384" text-anchor="middle">Ranked rows</text>
          <text class="t-sub" x="520" y="398" text-anchor="middle">primary keys</text>

          <rect class="s-mask" x="744" y="72" width="240" height="56" rx="6"/>
          <rect class="s-store" x="744" y="72" width="240" height="56" rx="6"/>
          <text class="t-name" x="864" y="96" text-anchor="middle">Stored vectors</text>
          <text class="t-sub" x="864" y="110" text-anchor="middle">old model space</text>

          <rect class="s-mask" x="744" y="216" width="240" height="56" rx="6"/>
          <rect class="s-focal" x="744" y="216" width="240" height="56" rx="6"/>
          <text class="t-name" x="864" y="240" text-anchor="middle">UniVec converter</text>
          <text class="t-sub" x="864" y="254" text-anchor="middle">source text stays put</text>

          <rect class="s-mask" x="744" y="360" width="240" height="56" rx="6"/>
          <rect class="s-store" x="744" y="360" width="240" height="56" rx="6"/>
          <text class="t-name" x="864" y="384" text-anchor="middle">Stored vectors</text>
          <text class="t-sub" x="864" y="398" text-anchor="middle">new model space</text>
          </svg>
        </figure>

        <dl class="fig-list">
          <div>
            <dt>Write path</dt>
            <dd>
              A committed row is enqueued by a trigger, embedded by the
              worker and written back to the vector column.
            </dd>
          </div>
          <div>
            <dt>Read path</dt>
            <dd>
              One call embeds the query, ranks both legs and fuses the
              two rankings.
            </dd>
          </div>
          <div>
            <dt>Model change</dt>
            <dd>
              Stored vectors convert into the new model's space in
              place. The source text stays put.
            </dd>
          </div>
        </dl>

        <p class="note note--wide">
          <a :href="withBase('/docs/concepts/')">How it works</a> covers
          the worker, the queue and the consistency window.
        </p>
      </section>

      <section class="band" aria-labelledby="example-heading">
        <div class="band__head band__head--row">
          <h2 id="example-heading">The SQL surface</h2>
          <p>
            Application work is SQL, in the <code>postvec</code> schema.
            The CLI configures the cluster and, in embedded mode, the
            models. Each tab is a starting state.
          </p>
        </div>

          <div class="tabs">
            <div class="tabs__bar" role="tablist" aria-label="Starting state">
              <button
                v-for="tab in tabs"
                :key="tab.id"
                type="button"
                role="tab"
                class="tabs__tab"
                :class="{ 'is-active': active === tab.id }"
                :aria-selected="active === tab.id"
                @click="active = tab.id"
              >
                {{ tab.label }}
              </button>
            </div>

            <div
              v-for="tab in tabs"
              v-show="active === tab.id"
              :key="tab.id"
              class="tabs__panel"
              role="tabpanel"
            >
              <p class="tabs__state">{{ tab.state }}</p>
              <div class="example-code vp-doc">
                <slot :name="tab.id" />
              </div>
              <p class="tabs__note">
                {{ tab.note }}
                <a :href="withBase(tab.href)">{{ tab.linkText }}</a>
                Long documents go through
                <a :href="withBase('/docs/guides/chunking')">recursive chunking</a>.
                <a :href="withBase('/docs/guides/templates')">Templates</a>
                control what text is sent for embedding.
                <a :href="withBase('/docs/reference/sql')">SQL reference</a>
                lists every function.
              </p>
            </div>
          </div>
      </section>

      <section class="band" aria-labelledby="modes-heading">
        <h2 id="modes-heading">Two inference modes, one SQL surface</h2>
        <div class="modes">
          <article>
            <h3>Embedded</h3>
            <p>
              The engine runs inside the PostgreSQL launcher. Text and
              weights stay on the database host, and you don't have to
              deploy anything else. Inference shares CPU, memory and
              failures with PostgreSQL. That's the trade.
            </p>
          </article>
          <article>
            <h3>Remote</h3>
            <p>
              Backends call ninference over gRPC, so inference can sit
              on its own CPU or GPU nodes. That's the shape for
              production at size. Mode is one cluster-wide setting. The
              SQL stays the same.
            </p>
          </article>
        </div>
        <p class="note note--wide">
          The extension, the CLI and their packages are under the
          PostgreSQL License. ninference is licensed separately, under
          community (non-commercial) and organisation (commercial)
          terms. Converter weights are a UniVec product: the public
          catalogue is a subset, and a verified account sees the
          private superset. The bundled MiniLM model needs neither an
          account nor a network.
          <a :href="withBase('/docs/concepts/modes')">Embedded vs remote</a>.
        </p>
      </section>

      <section class="band band--docs" aria-labelledby="docs-heading">
        <h2 id="docs-heading">Documentation</h2>
        <div class="index">
          <div>
            <h3>Install</h3>
            <ul>
              <li><a :href="withBase('/docs/quickstart')">Quick start</a></li>
              <li><a :href="withBase('/docs/install/docker')">Docker</a></li>
              <li><a :href="withBase('/docs/install/packages')">Packages</a></li>
              <li><a :href="withBase('/docs/install/setup')">Configure the cluster</a></li>
              <li><a :href="withBase('/docs/install/uninstall')">Uninstall</a></li>
            </ul>
          </div>
          <div>
            <h3>Use</h3>
            <ul>
              <li><a :href="withBase('/docs/guides/starting')">Which SQL call</a></li>
              <li><a :href="withBase('/docs/guides/search')">Search and filters</a></li>
              <li><a :href="withBase('/docs/guides/chunking')">Chunk long documents</a></li>
              <li><a :href="withBase('/docs/guides/migrate')">Migrate in place</a></li>
              <li><a :href="withBase('/docs/models/')">Models</a></li>
            </ul>
          </div>
          <div>
            <h3>Operate</h3>
            <ul>
              <li><a :href="withBase('/docs/guides/status')">Status and health</a></li>
              <li><a :href="withBase('/docs/troubleshooting')">Troubleshooting</a></li>
              <li><a :href="withBase('/docs/security')">Security and grants</a></li>
              <li><a :href="withBase('/docs/limits')">Limits</a></li>
              <li><a :href="withBase('/docs/faq')">FAQ</a></li>
            </ul>
          </div>
        </div>
        <p class="note">
          Release {{ SITE.version }} is {{ SITE.releaseStage }}. Search
          in the header jumps to a function, GUC or topic.
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
  width: min(72rem, calc(100% - 3.5rem));
  margin: 0 auto;
  padding: 3.75rem 0 5rem;
}

h1 {
  margin: 0;
  max-width: 20ch;
  font-family: var(--pv-font-display);
  font-size: clamp(2.05rem, 3.6vw, 2.85rem);
  font-weight: 500;
  letter-spacing: -0.03em;
  line-height: 1.12;
  text-wrap: balance;
}

h1::after {
  content: "";
  display: block;
  width: 2.4rem;
  height: 2px;
  margin-top: 1.1rem;
  background: var(--pv-mark);
}

.subhead {
  max-width: 38rem;
  margin: 1.05rem 0 0;
  font-size: 1.08rem;
  line-height: 1.5;
  color: var(--vp-c-text-2);
}

.lede {
  max-width: 38rem;
  margin: 1.1rem 0 0;
  font-size: 1.05rem;
  line-height: 1.65;
}

.lede--soft {
  color: var(--vp-c-text-2);
  font-size: 1rem;
}

.follow {
  margin: 0.95rem 0 0;
  color: var(--vp-c-text-1);
  font-size: 0.98rem;
  font-family: var(--pv-font-display);
  font-weight: 500;
  line-height: 1.5;
}

.hero {
  display: grid;
  grid-template-columns: minmax(0, 1.2fr) minmax(18rem, 0.8fr);
  gap: 3.25rem;
  align-items: start;
  padding-bottom: 3.2rem;
}

.hero__aside {
  margin-top: 0.35rem;
  padding-left: 1.85rem;
  border-left: 1px solid var(--vp-c-divider);
}

.defs {
  margin: 0;
}

.defs > div {
  padding: 1.05rem 0 1.1rem;
  border-bottom: 1px solid var(--vp-c-divider);
}

.defs dt {
  font-family: var(--pv-font-display);
  font-weight: 500;
  margin-bottom: 0.28rem;
}

.defs dd {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
  line-height: 1.55;
}

.hero__try {
  padding-top: 1.15rem;
}

.hero__try-label {
  margin: 0 0 0.45rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.7rem;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-3);
}

.links {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 0.4rem 1.35rem;
  margin-top: 1.55rem;
  font-size: 0.95rem;
}

.lead-link {
  font-weight: 600;
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

.band {
  padding: 2.5rem 0;
  border-top: 1px solid var(--vp-c-divider);
}

.home-page h2 {
  margin: 0 0 1.15rem;
  padding-top: 0;
  border-top: 0;
  font-family: var(--pv-font-display);
  font-size: 1.32rem;
  font-weight: 500;
  letter-spacing: -0.02em;
}

h3 {
  margin: 0 0 0.4rem;
  font-family: var(--pv-font-display);
  font-size: 1.02rem;
  font-weight: 500;
}

.band__head--row {
  margin-bottom: 0.25rem;
}

.band__head--row h2 {
  margin: 0 0 1.15rem;
}

.band__head--row p,
.band > p {
  margin: 0;
  max-width: 42rem;
  color: var(--vp-c-text-2);
  line-height: 1.65;
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
  margin: 0.85rem 0 0;
  font-size: 0.9rem;
  color: var(--vp-c-text-3);
  line-height: 1.55;
  max-width: none;
}

.band > p.note--wide {
  margin-top: 1.35rem;
  max-width: none;
}

.caps {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 0;
  margin: 0;
  border-top: 0;
}

.caps > div {
  padding: 1.15rem 1.5rem 0.25rem;
  border-right: 1px solid var(--vp-c-divider);
}

.caps > div:first-child {
  padding-left: 0;
}

.caps > div:last-child {
  padding-right: 0;
  border-right: 0;
}

.caps dt {
  font-family: var(--pv-font-display);
  font-weight: 500;
  margin-bottom: 0.45rem;
}

.caps dd {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
  line-height: 1.6;
}

.pvd {
  margin: 1.35rem 0 0.15rem;
}

.fig-list {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 1.5rem 2rem;
  margin: 1.15rem 0 0;
}

.fig-list dt {
  font-family: var(--pv-font-display);
  font-weight: 500;
  margin-bottom: 0.3rem;
}

.fig-list dd {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.92rem;
  line-height: 1.55;
}

.tabs {
  margin: 0.85rem 0 0.15rem;
}

.tabs__bar {
  display: flex;
  flex-wrap: wrap;
  gap: 0;
  border-bottom: 1px solid var(--vp-c-divider);
}

.tabs__tab {
  appearance: none;
  border: 0;
  border-bottom: 2px solid transparent;
  margin-bottom: -1px;
  background: transparent;
  color: var(--vp-c-text-3);
  padding: 0.5rem 0.85rem 0.5rem 0;
  margin-right: 0.85rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.74rem;
  letter-spacing: 0.03em;
  cursor: pointer;
}

.tabs__tab.is-active {
  color: var(--vp-c-brand-1);
  border-bottom-color: var(--vp-c-brand-1);
}

.tabs__tab:hover,
.tabs__tab:focus-visible {
  color: var(--vp-c-text-1);
}

.tabs__state {
  margin: 0.85rem 0 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
}

.tabs__note {
  margin: 1.05rem 0 0;
  color: var(--vp-c-text-3);
  font-size: 0.9rem;
  line-height: 1.6;
}

.example-code :deep(div[class*="language-"]) {
  margin: 0.75rem 0;
}

.modes {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 1.75rem 2.5rem;
  margin-top: 0;
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
  margin: 0.85rem 0 0.35rem;
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

@media (max-width: 980px) {
  .hero,
  .band__head--row {
    grid-template-columns: 1fr;
    gap: 1.6rem;
  }

  .hero__aside {
    margin-top: 0.4rem;
    padding-left: 0;
    border-left: 0;
    border-top: 1px solid var(--vp-c-divider);
    padding-top: 1.2rem;
  }

  .caps,
  .fig-list {
    grid-template-columns: 1fr;
    gap: 0;
  }

  .caps > div,
  .caps > div:first-child,
  .caps > div:last-child {
    padding: 0.95rem 0;
    border-right: 0;
    border-bottom: 1px solid var(--vp-c-divider);
  }

  .fig-list > div {
    padding: 0.85rem 0;
    border-top: 1px solid var(--vp-c-divider);
  }
}

@media (max-width: 720px) {
  .wrap {
    width: min(100% - 2rem, 72rem);
    padding-top: 2.75rem;
  }

  h1 {
    max-width: none;
  }

  .modes,
  .index {
    grid-template-columns: 1fr;
    gap: 1.15rem;
  }

  .pvd {
    display: none;
  }
}
</style>
