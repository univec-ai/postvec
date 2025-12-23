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
            <a :href="withBase('/docs/guides/starting')">Choose the SQL call</a>
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
            <dt>No API keys</dt>
            <dd>
              Embedding runs on open-weight models, either inside
              PostgreSQL or on inference nodes you operate. The
              database never holds a provider credential. In embedded
              mode the text stays on the host.
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

        <figure class="figure">
          <svg
            viewBox="0 0 760 252"
            role="img"
            aria-label="Three diagrams: the write path from source text to a vector column, the read path fusing semantic and full-text ranks, and the migration path converting stored vectors into a new model space."
          >
            <defs>
              <marker
                id="pv-arrow"
                viewBox="0 0 8 8"
                refX="7"
                refY="4"
                markerWidth="6"
                markerHeight="6"
                orient="auto-start-reverse"
              >
                <path class="head" d="M0 0 L8 4 L0 8 z" />
              </marker>
            </defs>

            <text class="lane" x="26" y="10">Write path</text>
            <rect class="b" x="26" y="22" width="132" height="38" />
            <text class="t" x="92" y="39">docs.body</text>
            <text class="s" x="92" y="52">source text</text>

            <text class="a" x="188" y="34">trigger</text>
            <line class="arrow" x1="162" y1="41" x2="214" y2="41" />

            <rect class="b" x="218" y="22" width="132" height="38" />
            <text class="t" x="284" y="39">postvec.jobs</text>
            <text class="s" x="284" y="52">queue</text>

            <text class="a" x="380" y="34">worker</text>
            <line class="arrow" x1="354" y1="41" x2="406" y2="41" />

            <rect class="b" x="410" y="22" width="132" height="38" />
            <text class="t" x="476" y="39">model</text>
            <text class="s" x="476" y="52">local or on-prem</text>

            <line class="arrow" x1="546" y1="41" x2="598" y2="41" />

            <rect class="b accent" x="602" y="22" width="132" height="38" />
            <text class="t" x="668" y="39">body_semantic</text>
            <text class="s" x="668" y="52">vector(384)</text>

            <text class="lane" x="26" y="86">Read path</text>
            <rect class="b" x="26" y="112" width="132" height="38" />
            <text class="t" x="92" y="129">query text</text>
            <text class="s" x="92" y="142">one call</text>

            <line class="arrow" x1="158" y1="127" x2="214" y2="115" />
            <line class="arrow" x1="158" y1="135" x2="214" y2="147" />

            <rect class="b" x="218" y="98" width="132" height="30" />
            <text class="t" x="284" y="117">semantic rank</text>

            <rect class="b" x="218" y="134" width="132" height="30" />
            <text class="t" x="284" y="153">full-text rank</text>

            <line class="arrow" x1="350" y1="113" x2="406" y2="127" />
            <line class="arrow" x1="350" y1="149" x2="406" y2="135" />

            <rect class="b" x="410" y="112" width="132" height="38" />
            <text class="t" x="476" y="129">fusion</text>
            <text class="s" x="476" y="142">reciprocal rank</text>

            <line class="arrow" x1="542" y1="131" x2="598" y2="131" />

            <rect class="b accent" x="602" y="112" width="132" height="38" />
            <text class="t" x="668" y="129">ranked rows</text>
            <text class="s" x="668" y="142">primary keys</text>

            <text class="lane" x="30" y="188">Model change</text>
            <rect class="b" x="30" y="200" width="180" height="38" />
            <text class="t" x="120" y="217">stored vectors</text>
            <text class="s" x="120" y="230">old model space</text>

            <text class="a" x="248" y="212">migrate()</text>
            <line class="arrow" x1="210" y1="219" x2="286" y2="219" />

            <rect class="b" x="290" y="200" width="180" height="38" />
            <text class="t" x="380" y="217">UniVec converter</text>
            <text class="s" x="380" y="230">source text stays put</text>

            <line class="arrow" x1="470" y1="219" x2="546" y2="219" />

            <rect class="b accent" x="550" y="200" width="180" height="38" />
            <text class="t" x="640" y="217">stored vectors</text>
            <text class="s" x="640" y="230">new model space</text>
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
              <li><a :href="withBase('/docs/guides/starting')">Choose the SQL call</a></li>
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

.figure {
  margin: 1.35rem 0 0.15rem;
}

.figure svg {
  display: block;
  width: 100%;
  height: auto;
  overflow: visible;
}

.figure .b {
  fill: var(--vp-c-bg-soft);
  stroke: var(--vp-c-divider);
  stroke-width: 1;
}

.figure .b.accent {
  stroke: var(--pv-mark);
}

.figure text {
  font-family: var(--vp-font-family-mono);
  text-anchor: middle;
}

.figure .lane {
  text-anchor: start;
  font-size: 10px;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  fill: var(--pv-mark);
}

.figure .t {
  font-size: 12px;
  fill: var(--vp-c-text-1);
}

.figure .s {
  font-size: 10px;
  fill: var(--vp-c-text-3);
}

.figure .a {
  font-size: 9.5px;
  letter-spacing: 0.04em;
  fill: var(--vp-c-text-3);
}

.figure .arrow {
  stroke: var(--vp-c-text-3);
  stroke-width: 1;
  marker-end: url(#pv-arrow);
}

.figure .head {
  fill: var(--vp-c-text-3);
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

  .figure {
    display: none;
  }
}
</style>
