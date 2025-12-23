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
    label: "Text, no vectors",
    state: "A text column that should become searchable.",
    note:
      "postvec adds a vector column beside the source, installs the triggers that keep it current, and fills it in the background.",
    href: "/docs/guides/enable",
    linkText: "Enable a column",
  },
  {
    id: "search",
    label: "Search it",
    state: "One call, ranked rows.",
    note:
      "Semantic and full-text candidates are filtered, ranked and fused together. The primary key comes back as text, so it joins to any table shape.",
    href: "/docs/guides/search",
    linkText: "Search and filters",
  },
  {
    id: "adopt",
    label: "Vectors already there",
    state: "A populated pgvector column, filled by application code.",
    note:
      "adopt() registers the column without rewriting a byte. If the model behind it is retired, queries are embedded with an available model and converted into that space.",
    href: "/docs/guides/adopt",
    linkText: "Adopt existing vectors",
  },
  {
    id: "migrate",
    label: "Change the model",
    state: "The stored space is the one that has to move.",
    note:
      "Stored vectors convert in batches into a new column while writes keep flowing. The columns swap at finalization; the source text is never re-read.",
    href: "/docs/guides/migrate",
    linkText: "Migrate in place",
  },
];

const active = ref(tabs[0].id);
</script>

<template>
  <main class="home-page">
    <div class="wrap">
      <header class="intro">
        <p class="kicker">
          PostgreSQL 16, 17 and 18 · PostgreSQL License ·
          {{ SITE.releaseStage }} {{ SITE.version }}
        </p>
        <h1>
          In-database embeddings, hybrid search and vector conversion
          for PostgreSQL
        </h1>
        <p class="lede">
          postvec makes a text column semantic. It creates and maintains a
          <code>pgvector</code> column beside your data, answers full-text
          and semantic queries together in a single call, and converts
          stored vectors from one embedding model's space into another
          without reading the source text again.
        </p>
        <p class="lede">
          That last part is the reason it exists. A knowledge base held as
          vectors is tied to the model that produced them, and providers
          retire a generation every year or two. The usual answer is to
          re-embed the corpus and rebuild the index — then to do it again
          at the next generation.
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
          postvec is where both are handled, in the database that already
          holds the text.
        </p>
        <nav class="links" aria-label="Primary documentation">
          <a class="lead-link" :href="withBase('/docs/quickstart')">Quick start</a>
          <a :href="withBase('/docs/install/')">Install</a>
          <a :href="withBase('/docs/guides/starting')">Choose the SQL call</a>
          <a :href="withBase('/download')">Downloads</a>
        </nav>
      </header>

      <section aria-labelledby="snapshot-heading">
        <h2 id="snapshot-heading">What it does</h2>
        <dl class="caps">
          <div>
            <dt>No API keys</dt>
            <dd>
              Embedding runs on open-weight models, either inside PostgreSQL
              or on inference nodes you operate. No provider credentials are
              stored in the database, and in embedded mode no text leaves the
              host.
            </dd>
          </div>
          <div>
            <dt>Knowledge bases that outlast a model</dt>
            <dd>
              Stored vectors convert between embedding spaces through a
              catalogue of {{ SITE.conversionPairs }} UniVec conversion
              pairs. The conversion reads the vectors, not the documents that
              produced them —
              <a :href="withBase('/docs/guides/migrate')">migrate in place</a>.
            </dd>
          </div>
          <div>
            <dt>Retired spaces stay searchable</dt>
            <dd>
              A column of <code>ada-002</code> vectors can be left exactly as
              it is. postvec embeds the query with a model that is still
              available and converts that single vector into the stored
              space —
              <a :href="withBase('/docs/guides/bridge')">bridge search</a>,
              no migration required.
            </dd>
          </div>
        </dl>
      </section>

      <section aria-labelledby="figure-heading">
        <h2 id="figure-heading">Three paths through the extension</h2>
        <p>
          Writes are asynchronous and driven by a background worker. Queries
          are synchronous. Model changes operate on the vectors, not on the
          corpus.
        </p>

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

            <!-- lane 1: write path -->
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

            <!-- lane 2: read path -->
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

            <!-- lane 3: model change -->
            <text class="lane" x="30" y="188">Model change</text>
            <rect class="b" x="30" y="200" width="180" height="38" />
            <text class="t" x="120" y="217">stored vectors</text>
            <text class="s" x="120" y="230">old model space</text>

            <text class="a" x="248" y="212">migrate()</text>
            <line class="arrow" x1="210" y1="219" x2="286" y2="219" />

            <rect class="b" x="290" y="200" width="180" height="38" />
            <text class="t" x="380" y="217">UniVec converter</text>
            <text class="s" x="380" y="230">source text not read</text>

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
              worker, and written back to the vector column.
            </dd>
          </div>
          <div>
            <dt>Read path</dt>
            <dd>
              One call embeds the query, ranks semantic and full-text
              candidates, and fuses the two rankings.
            </dd>
          </div>
          <div>
            <dt>Model change</dt>
            <dd>
              Stored vectors are converted into the new model's space in
              place. The source text is not read.
            </dd>
          </div>
        </dl>

        <p class="note">
          <a :href="withBase('/docs/concepts/')">How it works</a> covers the
          worker, the queue and the consistency window in detail.
        </p>
      </section>

      <section aria-labelledby="example-heading">
        <h2 id="example-heading">The SQL surface</h2>
        <p>
          Application work is SQL, in the <code>postvec</code> schema. The CLI
          configures the cluster and, in embedded mode, the models. Each tab
          below is a starting state.
        </p>

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
            </p>
          </div>
        </div>

        <p class="note">
          Long documents are split into passages by
          <a :href="withBase('/docs/guides/chunking')">recursive chunking</a>;
          <a :href="withBase('/docs/guides/templates')">templates</a> control
          what text is sent for embedding.
          <a :href="withBase('/docs/reference/sql')">The SQL reference</a>
          lists every function and argument.
        </p>
      </section>

      <section aria-labelledby="start-heading">
        <h2 id="start-heading">Try it on this machine</h2>
        <p>
          A disposable container is enough for a first look: PostgreSQL,
          pgvector, postvec and one bundled model, with no external service
          and no account. Tabs pick the PostgreSQL major, and the same
          choice is remembered on the install pages.
        </p>
        <PgSnippet
          id="docker-quickstart"
          caption="Wait until the container is healthy, then follow the quick start."
        />
        <p>
          For a real cluster there are
          <a :href="withBase('/docs/install/packages')">packages</a> for
          Debian, Ubuntu and EL9, a
          <a :href="withBase('/docs/install/source')">source build</a>, and a
          <a :href="withBase('/docs/install/setup')">one-command setup</a>
          that edits the cluster configuration for you. RDS and Aurora cannot
          load the worker.
        </p>
        <p class="note">
          Images live on GHCR and packages are GitHub Release assets. The
          <a :href="withBase('/download')">downloads page</a> lists the names
          and reports whether they are published yet.
        </p>
      </section>

      <section aria-labelledby="modes-heading">
        <h2 id="modes-heading">Two inference modes, one SQL surface</h2>
        <div class="modes">
          <article>
            <h3>Embedded</h3>
            <p>
              The engine runs inside the PostgreSQL launcher process. Text
              and model weights stay on the database host, and nothing else
              has to be deployed. The trade is deliberate: inference shares
              CPU, memory and the failure domain with PostgreSQL.
            </p>
          </article>
          <article>
            <h3>Remote</h3>
            <p>
              Backends call ninference over gRPC, so inference scales across
              CPU or GPU nodes and keeps its own failure domain. This is the
              shape for production at size. The mode is one cluster-wide
              setting; the SQL does not change.
            </p>
          </article>
        </div>
        <p class="note">
          The extension, the CLI and their packages are under the PostgreSQL
          License. ninference is licensed separately, under community
          (non-commercial) and organisation (commercial) terms. Converter
          weights are a UniVec product: the public catalogue is a subset, and
          a verified account sees the private superset. The bundled MiniLM
          model needs neither an account nor a network.
          <a :href="withBase('/docs/concepts/modes')">Embedded vs remote</a>
          compares the two.
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
          Release {{ SITE.version }} is {{ SITE.releaseStage }}. Search in
          the header jumps straight to a function, GUC or topic.
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
  width: min(50rem, calc(100% - 3rem));
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
  max-width: 22ch;
  font-family: var(--pv-font-display);
  font-size: clamp(2rem, 4.6vw, 2.85rem);
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

.lede + .lede {
  color: var(--vp-c-text-2);
  font-size: 1rem;
}

.follow {
  color: var(--vp-c-text-2);
  font-size: 1rem;
}

.defs {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 0;
  margin: 1.6rem 0 0;
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
  align-items: baseline;
  gap: 0.4rem 1.35rem;
  margin-top: 1.6rem;
  font-size: 0.95rem;
}

.lead-link {
  font-weight: 600;
}

.lead-link::after {
  content: " →";
  font-family: var(--vp-font-family-mono);
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
  padding: 2.6rem 0;
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
  max-width: 44rem;
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

/* diagram */

.figure {
  margin: 1.4rem 0 0.4rem;
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
  display: none;
  margin: 0.4rem 0 0;
}

/* tabs */

.tabs {
  margin: 1.1rem 0 0.4rem;
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
  padding: 0.5rem 0.9rem 0.5rem 0;
  margin-right: 0.9rem;
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
  margin: 0.9rem 0 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
}

.tabs__note {
  margin: 0.15rem 0 0;
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

@media (max-width: 760px) {
  .figure {
    display: none;
  }

  .fig-list {
    display: block;
  }

  .fig-list > div {
    padding: 0.8rem 0;
    border-top: 1px solid var(--vp-c-divider);
  }

  .fig-list > div:last-child {
    border-bottom: 1px solid var(--vp-c-divider);
  }

  .fig-list dt {
    font-weight: 600;
    margin-bottom: 0.25rem;
  }

  .fig-list dd {
    margin: 0;
    color: var(--vp-c-text-2);
    font-size: 0.95rem;
    line-height: 1.6;
  }
}

@media (max-width: 720px) {
  .wrap {
    width: min(100% - 2rem, 50rem);
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
