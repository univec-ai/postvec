<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";
import { withBase } from "vitepress";
import { SITE } from "../site";
import PgSnippet from "./PgSnippet.vue";
import OpenCoreStack from "./OpenCoreStack.vue";

/* ------------------------------------------------------------------ */
/* SQL walkthrough tabs                                                */
/* ------------------------------------------------------------------ */

type Tab = {
  id: string;
  label: string;
  title: string;
  body: string;
  expect: string;
  href: string;
  linkText: string;
};

const tabs: Tab[] = [
  {
    id: "enable",
    label: "enable()",
    title: "Declare a column semantic",
    body:
      "Creates the shadow vector column and registers the sync. Inserts and updates queue embedding work, and a background worker fills vectors after each commit.",
    expect:
      "The column appears at once; vectors fill in the background. Watch postvec.status() until pending_jobs reaches 0.",
    href: "/docs/guides/enable",
    linkText: "Enable guide",
  },
  {
    id: "search",
    label: "search()",
    title: "Hybrid search, one call",
    body:
      "Full-text and vector retrieval run together and the two rankings are fused. Typed metadata filters narrow the candidates. The primary key comes back as text, so it joins to any table.",
    expect:
      "Rows that are close in meaning rank highly, including when they share few keywords.",
    href: "/docs/guides/search",
    linkText: "Search guide",
  },
  {
    id: "adopt",
    label: "adopt()",
    title: "Take over existing vectors",
    body:
      "A column already filled by some other pipeline, even in a retired space like ada-002, is registered as it is. Queries are embedded with a local model and converted into that space.",
    expect: "Stored bytes untouched; search works on the next query.",
    href: "/docs/guides/adopt",
    linkText: "Adopt guide",
  },
  {
    id: "migrate",
    label: "migrate()",
    title: "Change models in place",
    body:
      "Stored vectors convert directly into the new model's space, drawing on a catalogue of " +
      SITE.conversionPairs +
      " conversion pairs. Finalize is a deliberate second step.",
    expect: "The old column keeps serving search until you finalize.",
    href: "/docs/guides/migrate",
    linkText: "Migrate guide",
  },
];

const active = ref(tabs[0].id);

function onTabKey(e: KeyboardEvent, idx: number) {
  const dir = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
  if (!dir) return;
  e.preventDefault();
  const next = (idx + dir + tabs.length) % tabs.length;
  active.value = tabs[next].id;
  const bar = (e.currentTarget as HTMLElement).parentElement;
  (bar?.children[next] as HTMLElement | undefined)?.focus();
}

/* ------------------------------------------------------------------ */
/* Topology figure                                                     */
/* ------------------------------------------------------------------ */
/* Three phases cycle in the hero. Dots travel on SMIL animateMotion,
   so no JavaScript runs per frame; the only timer is one setInterval
   for the phase change. The SVG is paused whenever it is scrolled out
   of view or the tab is hidden, and prefers-reduced-motion gets a
   static drawing of the full topology. */

type Phase = "a" | "b" | "c";
const PHASES: Phase[] = ["a", "b", "c"];
const PHASE_MS = 7000;

const captions: Record<Phase, { title: string; body: string }> = {
  a: {
    title: "Inference is embedded.",
    body:
      "Text is embedded inside PostgreSQL. Models on disk, no API key, nothing leaves the host.",
  },
  b: {
    title: "Inference on extra nodes.",
    body:
      "Optional postvec-server nodes take inference off the database host, over gRPC. Same SQL.",
  },
  c: {
    title: "Nodes form a cluster.",
    body:
      "Servers find each other by gossip. Add one and the fleet grows, with no reconfiguration.",
  },
};

const phase = ref<Phase>("a");
const animate = ref(true); // false under prefers-reduced-motion
const paused = ref(false);
const figure = ref<HTMLElement | null>(null);
const svg = ref<SVGSVGElement | null>(null);

let timer: number | undefined;
let observer: IntersectionObserver | undefined;
let inView = true;

function setPhase(p: Phase, restart = true) {
  phase.value = p;
  if (restart && animate.value) startTimer();
}

function startTimer() {
  stopTimer();
  timer = window.setInterval(() => {
    const i = PHASES.indexOf(phase.value);
    phase.value = PHASES[(i + 1) % PHASES.length];
  }, PHASE_MS);
}

function stopTimer() {
  if (timer !== undefined) {
    window.clearInterval(timer);
    timer = undefined;
  }
}

function syncRunning() {
  const shouldRun = animate.value && inView && !document.hidden;
  paused.value = !shouldRun;
  if (shouldRun) {
    svg.value?.unpauseAnimations();
    if (timer === undefined) startTimer();
  } else {
    svg.value?.pauseAnimations();
    stopTimer();
  }
}

function onVisibility() {
  syncRunning();
}

onMounted(() => {
  const mq = window.matchMedia("(prefers-reduced-motion: reduce)");
  if (mq.matches) {
    animate.value = false;
    phase.value = "c";
    return;
  }
  if ("IntersectionObserver" in window && figure.value) {
    observer = new IntersectionObserver(
      (entries) => {
        inView = entries[0]?.isIntersecting ?? true;
        syncRunning();
      },
      { threshold: 0.15 },
    );
    observer.observe(figure.value);
  }
  document.addEventListener("visibilitychange", onVisibility);
  syncRunning();
});

onBeforeUnmount(() => {
  stopTimer();
  observer?.disconnect();
  document.removeEventListener("visibilitychange", onVisibility);
});
</script>

<template>
  <main class="home-page">
    <!-- ============================================================ -->
    <!-- Hero                                                          -->
    <!-- ============================================================ -->
    <header class="hero">
      <div class="wrap hero__grid">
        <div class="hero__copy">
          <p class="hero__kicker">
            <span class="tag">PostgreSQL License</span>
            Open-source extension for PostgreSQL
          </p>
          <h1>Evergreen hybrid search for PostgreSQL</h1>
          <p class="subhead">
            Full-text and semantic search in one call. Embedding and
            vector conversion inside the database with local models or
            via external providers.
          </p>
          <div class="hero__cta">
            <a class="btn btn--go" :href="withBase('/docs/quickstart')">Quick start</a>
            <a class="btn" :href="withBase('/docs/')">Documentation</a>
          </div>
          <p class="hero__fine">
            Local inference by default. No API key. Compatible with
            PostgreSQL 16, 17 and 18.
          </p>
        </div>

        <figure
          ref="figure"
          class="topo"
          :data-phase="phase"
          :class="{ 'is-static': !animate, 'is-paused': paused }"
        >
          <svg
            ref="svg"
            viewBox="0 0 760 330"
            xmlns="http://www.w3.org/2000/svg"
            role="img"
            aria-labelledby="topo-title topo-desc"
          >
            <title id="topo-title">Where inference runs</title>
            <desc id="topo-desc">
              A PostgreSQL database holding a text column and a shadow vector
              column, with an embedded inference engine. Optional
              postvec-server nodes take inference off the host over gRPC and
              discover each other by gossip.
            </desc>

            <!-- database -->
            <g class="db">
              <path
                class="db__body"
                d="M 60 70 a 110 26 0 0 1 220 0 v 190 a 110 26 0 0 1 -220 0 z"
              />
              <ellipse class="db__lid" cx="170" cy="70" rx="110" ry="26" />
              <text class="t-name" x="170" y="76" text-anchor="middle">PostgreSQL</text>
            </g>

            <!-- table rows -->
            <g class="rows">
              <rect class="row" x="88" y="112" width="164" height="24" rx="4" />
              <text class="t-mono" x="98" y="128">body</text>
              <text class="t-mono t-type" x="242" y="128" text-anchor="end">text</text>

              <rect class="row row--vec" x="88" y="142" width="164" height="24" rx="4" />
              <text class="t-mono t-accent" x="98" y="158">body_semantic</text>
              <text class="t-mono t-accent" x="242" y="158" text-anchor="end">vector</text>
            </g>

            <!-- embedded inference chip -->
            <g class="chip">
              <rect class="chip__body" x="88" y="184" width="164" height="46" rx="6" />
              <rect
                v-if="animate"
                class="chip__pulse"
                x="88"
                y="184"
                width="164"
                height="46"
                rx="6"
              />
              <text class="t-chip" x="170" y="203" text-anchor="middle">embedded inference</text>
              <text class="t-chip-sub" x="170" y="219" text-anchor="middle">
                models on disk · no API key
              </text>
            </g>

            <!-- rails inside the database (invisible, dots ride them) -->
            <path id="topo-rail-down" d="M 132 138 C 108 158, 108 172, 128 186" fill="none" stroke="none" />
            <path id="topo-rail-up" d="M 212 186 C 232 172, 232 162, 212 148" fill="none" stroke="none" />

            <!-- gRPC links -->
            <g class="grpc">
              <path id="topo-g1" class="wire" d="M 285 130 C 355 88, 410 43, 474 43" />
              <path id="topo-g2" class="wire" d="M 285 165 C 370 168, 460 165, 579 165" />
              <path id="topo-g3" class="wire" d="M 285 200 C 355 244, 410 287, 474 287" />
              <text class="t-wire" x="375" y="150" text-anchor="middle">gRPC</text>
            </g>

            <!-- cluster mesh -->
            <g class="mesh">
              <path id="topo-c12" class="wire wire--mesh" d="M 566 75 C 600 89, 626 109, 642 133" />
              <path id="topo-c23" class="wire wire--mesh" d="M 642 197 C 626 221, 600 241, 566 255" />
              <path id="topo-c31" class="wire wire--mesh" d="M 536 253 L 536 77" />
              <text class="t-wire t-accent" x="688" y="105" text-anchor="middle">gossip</text>
            </g>

            <!-- server nodes -->
            <g class="servers">
              <g class="node n1">
                <rect x="478" y="15" width="118" height="60" rx="6" />
                <circle cx="492" cy="29" r="3" />
                <text class="t-node" x="537" y="45" text-anchor="middle">postvec-server</text>
                <text class="t-node-sub" x="537" y="61" text-anchor="middle">inference node</text>
              </g>
              <g class="node n2">
                <rect x="583" y="135" width="118" height="60" rx="6" />
                <circle cx="597" cy="149" r="3" />
                <text class="t-node" x="642" y="165" text-anchor="middle">postvec-server</text>
                <text class="t-node-sub" x="642" y="181" text-anchor="middle">inference node</text>
              </g>
              <g class="node n3">
                <rect x="478" y="255" width="118" height="60" rx="6" />
                <circle cx="492" cy="269" r="3" />
                <text class="t-node" x="537" y="285" text-anchor="middle">postvec-server</text>
                <text class="t-node-sub" x="537" y="301" text-anchor="middle">inference node</text>
              </g>
            </g>

            <!-- traveling dots: declarative, no per-frame script -->
            <g v-if="animate" class="dots">
              <!-- embedded loop: text down to the engine, vector back up -->
              <g class="dots--local">
              <circle class="dot dot--in" r="3" opacity="0">
                <animateMotion dur="1.4s" begin="0s" repeatCount="indefinite" keyPoints="0;1" keyTimes="0;1" calcMode="linear">
                  <mpath href="#topo-rail-down" />
                </animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.15;0.85;1" dur="1.4s" begin="0s" repeatCount="indefinite" />
              </circle>
              <circle class="dot dot--out" r="3" opacity="0">
                <animateMotion dur="1.4s" begin="1.5s" repeatCount="indefinite" keyPoints="0;1" keyTimes="0;1" calcMode="linear">
                  <mpath href="#topo-rail-up" />
                </animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.15;0.85;1" dur="1.4s" begin="1.5s" repeatCount="indefinite" />
              </circle>
              </g>

              <!-- gRPC round trips, one per link, staggered -->
              <g class="dots--grpc">
                <circle class="dot dot--in" r="3" opacity="0">
                  <animateMotion dur="1.6s" begin="0.2s" repeatCount="indefinite" keyPoints="0;1" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-g1" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.6s" begin="0.2s" repeatCount="indefinite" />
                </circle>
                <circle class="dot dot--out" r="3" opacity="0">
                  <animateMotion dur="1.6s" begin="1.9s" repeatCount="indefinite" keyPoints="1;0" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-g1" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.6s" begin="1.9s" repeatCount="indefinite" />
                </circle>

                <circle class="dot dot--in" r="3" opacity="0">
                  <animateMotion dur="1.6s" begin="1.3s" repeatCount="indefinite" keyPoints="0;1" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-g2" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.6s" begin="1.3s" repeatCount="indefinite" />
                </circle>
                <circle class="dot dot--out" r="3" opacity="0">
                  <animateMotion dur="1.6s" begin="3s" repeatCount="indefinite" keyPoints="1;0" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-g2" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.6s" begin="3s" repeatCount="indefinite" />
                </circle>

                <circle class="dot dot--in" r="3" opacity="0">
                  <animateMotion dur="1.6s" begin="2.4s" repeatCount="indefinite" keyPoints="0;1" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-g3" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.6s" begin="2.4s" repeatCount="indefinite" />
                </circle>
                <circle class="dot dot--out" r="3" opacity="0">
                  <animateMotion dur="1.6s" begin="4.1s" repeatCount="indefinite" keyPoints="1;0" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-g3" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.6s" begin="4.1s" repeatCount="indefinite" />
                </circle>
              </g>

              <!-- gossip: small accent dots around the mesh -->
              <g class="dots--mesh">
                <circle class="dot dot--out" r="2.4" opacity="0">
                  <animateMotion dur="1.2s" begin="0s" repeatCount="indefinite" keyPoints="0;1" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-c12" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.15;0.85;1" dur="1.2s" begin="0s" repeatCount="indefinite" />
                </circle>
                <circle class="dot dot--out" r="2.4" opacity="0">
                  <animateMotion dur="1.2s" begin="0.9s" repeatCount="indefinite" keyPoints="0;1" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-c23" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.15;0.85;1" dur="1.2s" begin="0.9s" repeatCount="indefinite" />
                </circle>
                <circle class="dot dot--out" r="2.4" opacity="0">
                  <animateMotion dur="1.2s" begin="1.8s" repeatCount="indefinite" keyPoints="1;0" keyTimes="0;1" calcMode="linear">
                    <mpath href="#topo-c31" />
                  </animateMotion>
                  <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.15;0.85;1" dur="1.2s" begin="1.8s" repeatCount="indefinite" />
                </circle>
              </g>
            </g>
          </svg>

          <figcaption class="topo__caps">
            <div
              v-for="p in PHASES"
              :key="p"
              class="topo__cap"
              :class="{ 'is-on': phase === p }"
              :aria-hidden="phase !== p"
            >
              <h3>{{ captions[p].title }}</h3>
              <p>{{ captions[p].body }}</p>
            </div>
          </figcaption>

          <div v-if="animate" class="topo__steps" role="group" aria-label="Topology phases">
            <button
              v-for="(p, i) in PHASES"
              :key="p"
              type="button"
              class="topo__step"
              :class="{
                'is-on': phase === p,
                'is-done': PHASES.indexOf(phase) > i,
              }"
              :style="{ '--stepdur': PHASE_MS / 1000 + 's' }"
              :aria-label="captions[p].title"
              :aria-pressed="phase === p"
              @click="setPhase(p)"
            />
          </div>
        </figure>
      </div>

      <ul class="wrap facts" aria-label="postvec at a glance">
        <li><b>16, 17, 18</b> PostgreSQL majors, pgvector 0.8+</li>
        <li><b>One call</b> for BM25 and vector search</li>
        <li><b>{{ SITE.conversionPairs }}</b> conversion pairs</li>
        <li><b>7</b> hosted providers, keys on the inference host</li>
      </ul>
    </header>

    <!-- ============================================================ -->
    <!-- Lock-in and what postvec does                                 -->
    <!-- ============================================================ -->
    <section class="band band--story" aria-labelledby="story-heading">
      <div class="wrap">
        <h2 id="story-heading" class="visually-hidden">
          Vector lock-in and embedding debt
        </h2>
        <p class="story">
          A knowledge base held as vectors is tied to the model that
          produced them. Providers retire a model generation every year
          or two, and each change means re-embedding the corpus and
          rebuilding the index.
        </p>

        <dl class="defs">
          <div>
            <dt>Vector lock-in</dt>
            <dd>
              The dependency between stored vectors and the model that
              produced them. postvec can translate a search query into an
              older model's space, so a deprecated index keeps answering
              with no migration.
            </dd>
          </div>
          <div>
            <dt>Embedding debt</dt>
            <dd>
              The cost of changing that dependency later. postvec migrates
              a vector store straight into a newer model's space, using
              {{ SITE.conversionPairs }} conversion pairs from the UniVec
              catalogue.
            </dd>
          </div>
        </dl>

        <p class="product">
          postvec makes a text column semantic. It keeps a shadow
          <code>pgvector</code> column in sync with the text, fuses
          full-text and semantic search in one call and converts stored
          vectors from one model space into another directly.
        </p>

        <div class="pledges">
          <article class="pledge">
            <h3>Zero API keys</h3>
            <p>
              Swappable embedding models run locally, inside PostgreSQL or
              on inference nodes you operate. Raw text stays on hosts you
              control. Hosted providers are opt-in per column, and keys stay
              in the inference layer.
            </p>
            <a :href="withBase('/docs/models/providers')">External providers</a>
          </article>
          <article class="pledge">
            <h3>Evergreen knowledge bases</h3>
            <p>
              Convert existing vectors between embedding spaces through the
              UniVec catalogue of {{ SITE.conversionPairs }} pairs. The
              source text stays where it is, and search keeps answering
              while the migration runs.
            </p>
            <a :href="withBase('/docs/guides/migrate')">Migrate in place</a>
          </article>
          <article class="pledge">
            <h3>Deprecated spaces keep working</h3>
            <p>
              Bridge search embeds the query with a local model, then
              converts that one vector into the stored space. An
              <code>ada-002</code> index answers as if nothing changed.
            </p>
            <a :href="withBase('/docs/guides/bridge')">Search a retired space</a>
          </article>
        </div>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- The SQL surface                                               -->
    <!-- ============================================================ -->
    <section class="band" aria-labelledby="sql-heading">
      <div class="wrap">
        <h2 id="sql-heading">Enable, search, adopt, migrate.</h2>
        <p class="prose">Dedicated SQL functions per usage scenario.</p>

        <div class="try">
          <div class="try__head">
            <p class="try__label">Try it</p>
            <p class="try__hint">
              One container with PostgreSQL, pgvector, postvec and the
              bundled MiniLM model.
            </p>
          </div>
          <PgSnippet id="docker-quickstart" />
        </div>

        <div class="tabs">
          <div class="tabs__bar" role="tablist" aria-label="SQL walkthrough">
            <button
              v-for="(tab, i) in tabs"
              :id="'tab-' + tab.id"
              :key="tab.id"
              type="button"
              role="tab"
              class="tabs__tab"
              :class="{ 'is-active': active === tab.id }"
              :aria-selected="active === tab.id"
              :aria-controls="'panel-' + tab.id"
              :tabindex="active === tab.id ? 0 : -1"
              @click="active = tab.id"
              @keydown="onTabKey($event, i)"
            >
              {{ tab.label }}
            </button>
          </div>

          <div
            v-for="tab in tabs"
            v-show="active === tab.id"
            :id="'panel-' + tab.id"
            :key="tab.id"
            class="tabs__panel"
            role="tabpanel"
            :aria-labelledby="'tab-' + tab.id"
          >
            <div class="tabs__code vp-doc">
              <slot :name="tab.id" />
            </div>
            <div class="tabs__why">
              <h3>{{ tab.title }}</h3>
              <p>{{ tab.body }}</p>
              <p class="expect"><b>Expected:</b> {{ tab.expect }}</p>
              <a class="more" :href="withBase(tab.href)">{{ tab.linkText }} &rarr;</a>
            </div>
          </div>
        </div>

        <p class="note">
          Long documents go through
          <a :href="withBase('/docs/guides/chunking')">recursive chunking</a>,
          <a :href="withBase('/docs/guides/templates')">templates</a> control
          what text is sent for embedding and the
          <a :href="withBase('/docs/reference/sql')">SQL reference</a> lists
          every function.
        </p>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- Open core                                                     -->
    <!-- ============================================================ -->
    <section class="band band--alt" aria-labelledby="core-heading">
      <div class="wrap core">
        <div>
          <p class="eyebrow">Open core</p>
          <h2 id="core-heading">Open source, with an optional server</h2>
          <div class="prose">
            <p>
              The extension, the CLI, the embedded inference engine and the
              packages are released under the PostgreSQL License, for any use.
              On a self-hosted cluster, the extension alone runs the complete SQL
              surface.
            </p>
            <p>
              postvec-server is a source-available companion. It moves
              inference into a separate process or onto a CPU and GPU fleet,
              adds a dashboard for model management and runs postvec on managed
              databases such as Amazon RDS, Cloud SQL and Supabase.
            </p>
          </div>
          <div class="core__links">
            <a class="btn" :href="withBase('/server')">postvec-server</a>
            <a :href="withBase('/docs/license')">License</a>
          </div>
        </div>
        <OpenCoreStack compact />
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- Documentation                                                 -->
    <!-- ============================================================ -->
    <section class="band" aria-labelledby="map-heading">
      <div class="wrap">
        <h2 id="map-heading">Documentation</h2>
        <div class="dir">
          <div class="dir__card">
            <h3>Get running</h3>
            <a :href="withBase('/docs/quickstart')">
              Quick start
              <small>one container with PostgreSQL, postvec and MiniLM</small>
            </a>
            <a :href="withBase('/docs/install/docker')">
              Docker
              <small>images for pg 16 / 17 / 18</small>
            </a>
            <a :href="withBase('/docs/install/packages')">
              Packages
              <small>.deb and .rpm, checksummed</small>
            </a>
            <a :href="withBase('/docs/install/setup')">
              Configure
              <small>postvec setup, then one restart</small>
            </a>
            <a :href="withBase('/docs/install/uninstall')">Uninstall</a>
          </div>
          <div class="dir__card">
            <h3>Day to day</h3>
            <a :href="withBase('/docs/guides/')">
              SQL functions
              <small>enable, adopt, bridge or migrate</small>
            </a>
            <a :href="withBase('/docs/guides/search')">Search and filters</a>
            <a :href="withBase('/docs/guides/migrate')">Migrate in place</a>
            <a :href="withBase('/docs/guides/bridge')">Search a retired space</a>
            <a :href="withBase('/docs/guides/status')">Observe the worker</a>
          </div>
          <div class="dir__card">
            <h3>Models and reference</h3>
            <a :href="withBase('/docs/models/')">
              Manage models
              <small>pull / activate / upgrade</small>
            </a>
            <a :href="withBase('/docs/server/')">
              Remote inference
              <small>nodes, fleets, gossip</small>
            </a>
            <a :href="withBase('/docs/reference/sql')">SQL reference</a>
            <a :href="withBase('/docs/reference/cli')">CLI reference</a>
            <a :href="withBase('/docs/troubleshooting')">Troubleshooting</a>
          </div>
        </div>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- UniVec                                                        -->
    <!-- ============================================================ -->
    <section class="band band--univec" aria-labelledby="univec-heading">
      <div class="wrap univec">
        <div>
          <p class="eyebrow">UniVec</p>
          <h2 id="univec-heading">Made by UniVec</h2>
        </div>
        <div>
          <p class="univec__text">
            UniVec builds embedding translation infrastructure in Dublin,
            Ireland, and develops postvec. The converters behind
            <code>adopt()</code> and <code>migrate()</code> come from the UniVec
            catalogue. UniVec also runs a hosted embedding and conversion API,
            which postvec can use as an external provider, and offers support
            agreements for production deployments.
          </p>
          <div class="univec__links">
            <a :href="SITE.univec" target="_blank" rel="noopener">univec.ai</a>
            <a :href="`${SITE.univec}/pricing`" target="_blank" rel="noopener">Hosted API pricing</a>
            <a :href="withBase('/server#plans')">postvec pro</a>
            <a :href="`${SITE.univec}/contact`" target="_blank" rel="noopener">Contact</a>
          </div>
        </div>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- Worth knowing                                                 -->
    <!-- ============================================================ -->
    <section class="band band--last" aria-labelledby="know-heading">
      <div class="wrap">
        <h2 id="know-heading">Requirements</h2>
        <div class="know">
          <p>
            You need PostgreSQL 16, 17 or 18 on a host where you can set
            <code>shared_preload_libraries</code>. A restart is part of
            first-time setup. Vectors fill after commit, not inside the
            inserting transaction.
          </p>
          <p>
            Inference runs embedded in PostgreSQL or on remote
            <code>postvec-server</code> nodes; the mode is one cluster-wide
            setting and the SQL is the same in both.
            <a :href="withBase('/docs/concepts/modes')">Embedded vs remote</a>
            has the trade-offs.
          </p>
          <p>
            The extension, the CLI and their packages are under the
            PostgreSQL License. The bundled MiniLM model works offline;
            converter weights are a separate UniVec product, with a public
            catalogue and a larger private one for verified accounts.
            Release {{ SITE.version }} is {{ SITE.releaseStage }}.
          </p>
        </div>
      </div>
    </section>
  </main>
</template>

<style scoped>
/* ------------------------------------------------------------------ */
/* Page frame                                                          */
/* ------------------------------------------------------------------ */

.home-page {
  --home-radius: 6px;
  --home-ink-line: var(--vp-c-border);
  color: var(--vp-c-text-1);
  font-size: 1rem;
  line-height: 1.5;
}

.wrap {
  width: min(72rem, calc(100% - 3.5rem));
  margin: 0 auto;
}

a {
  color: var(--vp-c-brand-1);
  text-decoration: none;
}

a:hover,
a:focus-visible {
  text-decoration: underline;
  text-underline-offset: 0.12em;
}

code {
  font-family: var(--vp-font-family-mono);
  font-size: 0.85em;
}

p code,
dd code {
  padding: 0.2em 0.4em;
  background: var(--vp-code-bg);
  color: var(--vp-c-text-1);
  border: 0;
  border-radius: var(--home-radius);
}

.visually-hidden {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
}

/* ------------------------------------------------------------------ */
/* Hero                                                                */
/* ------------------------------------------------------------------ */

.hero {
  position: relative;
  padding: 3.75rem 0 3.5rem;
  border-bottom: 1px solid var(--vp-c-divider);
  overflow: hidden;
}

/* faint grid behind the hero, fading out from the figure side */
.hero::before {
  content: "";
  position: absolute;
  inset: 0;
  background-image:
    linear-gradient(var(--vp-c-border-soft) 1px, transparent 1px),
    linear-gradient(90deg, var(--vp-c-border-soft) 1px, transparent 1px);
  background-size: 28px 28px;
  mask-image: radial-gradient(65% 85% at 72% 45%, #000 15%, transparent 72%);
  pointer-events: none;
}

.hero::after {
  content: "";
  position: absolute;
  width: 36rem;
  height: 36rem;
  right: -8rem;
  top: -12rem;
  background: radial-gradient(circle, color-mix(in srgb, var(--pv-mark) 13%, transparent), transparent 65%);
  pointer-events: none;
}

.hero__grid { position: relative; z-index: 1; }

.hero__kicker {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.6rem;
  margin: 0 0 1.1rem;
  font-size: 0.86rem;
  color: var(--vp-c-text-2);
}

.tag {
  padding: 0.12rem 0.45rem;
  border: 1px solid color-mix(in srgb, var(--pv-mark) 45%, var(--vp-c-border));
  border-radius: 4px;
  background: var(--vp-c-brand-soft);
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  color: var(--pv-mark);
}

/* ------------------------------------------------------------------ */
/* Facts strip — a quiet line under the hero, same surface             */
/* ------------------------------------------------------------------ */

.facts {
  position: relative;
  z-index: 1;
  display: flex;
  flex-wrap: wrap;
  gap: 0.35rem 1.75rem;
  margin-top: 2.1rem;
  padding: 0.85rem 0 0;
  border-top: 1px solid var(--vp-c-divider);
  list-style: none;
  font-size: 0.8rem;
  line-height: 1.45;
  color: var(--vp-c-text-3);
}

.facts li { margin: 0; }

.facts b {
  font-family: var(--vp-font-family-mono);
  font-weight: 500;
  color: var(--vp-c-text-2);
}

/* ------------------------------------------------------------------ */
/* Open core and UniVec                                                */
/* ------------------------------------------------------------------ */

.eyebrow {
  margin: 0 0 0.6rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--pv-mark);
}

.core {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 3rem;
  align-items: center;
}

.core .prose { line-height: 1.65; }

.core__links {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.6rem 1.25rem;
  margin-top: 1.4rem;
  font-size: 0.92rem;
  font-weight: 600;
}

.band--univec {
  background:
    radial-gradient(55% 100% at 0% 0%, color-mix(in srgb, var(--pv-mark) 8%, transparent), transparent 70%),
    var(--vp-c-bg);
}

.univec {
  display: grid;
  grid-template-columns: minmax(0, 0.8fr) minmax(0, 1.2fr);
  gap: 1rem 3rem;
  align-items: start;
}

.univec__text {
  margin: 0;
  font-size: 1.02rem;
  line-height: 1.7;
  color: var(--vp-c-text-2);
}

.univec__links {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem 1.4rem;
  margin-top: 1.1rem;
  font-size: 0.92rem;
  font-weight: 600;
}

.hero__grid {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 3rem;
  align-items: center;
}

h1 {
  margin: 0;
  font-family: var(--pv-font-display);
  font-size: clamp(2.1rem, 4.4vw, 3.15rem);
  font-weight: 500;
  letter-spacing: -0.025em;
  line-height: 1.08;
  text-wrap: balance;
}

.subhead {
  max-width: 34em;
  margin: 1.25rem 0 0;
  font-size: 1.08rem;
  line-height: 1.55;
  color: var(--vp-c-text-2);
}

.hero__cta {
  display: flex;
  flex-wrap: wrap;
  gap: 0.7rem;
  margin-top: 1.7rem;
}

.btn {
  display: inline-block;
  padding: 0.45rem 1rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--home-radius);
  background: var(--vp-c-bg-alt);
  color: var(--vp-c-text-1);
  font-size: 0.875rem;
  font-weight: 500;
  line-height: 1.4;
  box-shadow: var(--vp-shadow-1);
}

.btn:hover,
.btn:focus-visible {
  text-decoration: none;
  background: var(--vp-c-bg-soft);
  border-color: var(--vp-c-text-3);
}

.btn--go {
  background: var(--vp-button-brand-bg);
  border-color: var(--vp-button-brand-border, var(--vp-button-brand-bg));
  color: var(--vp-button-brand-text);
}

.btn--go:hover,
.btn--go:focus-visible {
  background: var(--vp-button-brand-hover-bg);
  border-color: var(--vp-button-brand-hover-bg);
  color: var(--vp-button-brand-hover-text);
}

.hero__fine {
  margin: 1.1rem 0 0;
  font-size: 0.82rem;
  color: var(--vp-c-text-3);
}

/* ------------------------------------------------------------------ */
/* Topology figure                                                     */
/* ------------------------------------------------------------------ */

.topo {
  margin: 0;
  padding: 1.5rem 1.25rem 1rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--home-radius);
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
}

.topo svg {
  display: block;
  width: 100%;
  height: auto;
  overflow: visible;
}

/* drawing */
.topo .db__body { fill: var(--vp-c-bg-elv); stroke: var(--vp-c-text-1); stroke-width: 2; }
.topo .db__lid { fill: var(--vp-c-bg-soft); stroke: var(--vp-c-text-1); stroke-width: 2; }
.topo .row { fill: var(--vp-c-bg); stroke: var(--vp-c-divider); }
.topo .row--vec { fill: var(--vp-c-brand-soft); stroke: var(--vp-c-brand-1); stroke-opacity: 0.45; }
.topo .chip__body { fill: var(--pv-field); }
.topo .chip__pulse { fill: none; stroke: var(--pv-mark); stroke-width: 2; animation: chip 3.2s ease-in-out infinite; }
.topo .servers rect { fill: var(--vp-c-bg-elv); stroke: var(--pv-field); stroke-width: 1.6; }
.topo .servers circle { fill: var(--pv-field); }
.topo .wire { fill: none; stroke: var(--vp-c-text-3); stroke-width: 1.5; stroke-dasharray: 6 6; animation: flow 1.2s linear infinite; }
.topo .wire--mesh { stroke: var(--pv-mark); stroke-width: 1.4; stroke-dasharray: 3 7; animation-duration: 1.6s; }
.topo .dot--in { fill: var(--pv-field); }
.topo .dot--out { fill: var(--pv-mark); }

.topo text { font-family: var(--vp-font-family-base); }
.topo .t-name { font-size: 13px; font-weight: 600; fill: var(--vp-c-text-1); }
.topo .t-mono { font-family: var(--vp-font-family-mono); font-size: 11px; fill: var(--vp-c-text-2); }
.topo .t-type { fill: var(--pv-field); }
.topo .t-accent { fill: var(--pv-mark); }
.topo .t-chip { font-size: 12px; font-weight: 600; fill: var(--vp-button-brand-text); }
.topo .t-chip-sub { font-size: 10px; fill: var(--vp-button-brand-text); opacity: 0.78; }
.topo .t-node { font-size: 11.5px; font-weight: 600; fill: var(--vp-c-text-1); }
.topo .t-node-sub { font-size: 9.5px; fill: var(--vp-c-text-3); }
.topo .t-wire { font-family: var(--vp-font-family-mono); font-size: 10px; fill: var(--vp-c-text-3); }

.dark .topo .chip__body { fill: #238636; }
.dark .topo .t-chip,
.dark .topo .t-chip-sub { fill: #ffffff; }

/* phase transitions. visibility is transitioned alongside opacity so a
   hidden group is skipped by the painter, not just drawn transparent. */
.topo .servers .node,
.topo .grpc,
.topo .mesh,
.topo .chip,
.topo .dots--local,
.topo .dots--grpc,
.topo .dots--mesh {
  transition:
    opacity 1s cubic-bezier(0.22, 1, 0.36, 1),
    transform 1s cubic-bezier(0.22, 1, 0.36, 1),
    visibility 1s;
}

.topo .servers .node { transform-origin: center; transform-box: fill-box; }

.topo[data-phase="a"] .servers .node { opacity: 0; visibility: hidden; transform: translateX(16px); }
.topo[data-phase="a"] .grpc,
.topo[data-phase="a"] .dots--grpc,
.topo[data-phase="a"] .mesh,
.topo[data-phase="a"] .dots--mesh { opacity: 0; visibility: hidden; }

.topo[data-phase="b"] .mesh,
.topo[data-phase="b"] .dots--mesh { opacity: 0; visibility: hidden; }

/* once nodes exist, inference is fully remote: the embedded chip goes */
.topo .chip { transform-origin: center; transform-box: fill-box; }
.topo[data-phase="b"] .chip,
.topo[data-phase="c"] .chip,
.topo[data-phase="b"] .dots--local,
.topo[data-phase="c"] .dots--local { opacity: 0; visibility: hidden; transform: scale(0.94); }
.topo[data-phase="b"] .servers .node { transition-delay: 0.35s; }
.topo[data-phase="b"] .servers .n2 { transition-delay: 0.53s; }
.topo[data-phase="b"] .servers .n3 { transition-delay: 0.71s; }

/* paused when out of view or tab hidden; SMIL is paused from script */
.topo.is-paused .wire,
.topo.is-paused .chip__pulse { animation-play-state: paused; }

/* static under prefers-reduced-motion */
.topo.is-static .wire { animation: none; }
.topo.is-static .servers .node,
.topo.is-static .grpc,
.topo.is-static .mesh,
.topo.is-static .chip { transition: none; }

@keyframes flow { to { stroke-dashoffset: -12; } }
@keyframes chip {
  0%, 100% { opacity: 0; }
  18% { opacity: 0.55; }
  36% { opacity: 0; }
}

/* captions */
.topo__caps {
  position: relative;
  min-height: 5.6rem;
  margin-top: 0.6rem;
  padding-top: 0.75rem;
  border-top: 1px solid var(--vp-c-divider);
  text-align: center;
}

.topo__cap {
  position: absolute;
  inset: 0.75rem 0 0;
  opacity: 0;
  transform: translateY(6px);
  transition:
    opacity 0.9s cubic-bezier(0.22, 1, 0.36, 1),
    transform 0.9s cubic-bezier(0.22, 1, 0.36, 1);
  pointer-events: none;
}

.topo__cap.is-on { opacity: 1; transform: translateY(0); pointer-events: auto; }

.topo__cap h3 {
  margin: 0;
  font-family: var(--vp-font-family-base);
  font-size: 1rem;
  font-weight: 600;
  color: var(--vp-c-text-1);
}

.topo__cap p {
  margin: 0.2rem auto 0;
  max-width: 34em;
  font-size: 0.86rem;
  line-height: 1.5;
  color: var(--vp-c-text-2);
}

.topo.is-static .topo__cap { transition: none; }

/* step bars */
.topo__steps {
  display: flex;
  justify-content: center;
  gap: 0.6rem;
  margin-top: 0.35rem;
}

.topo__step {
  position: relative;
  width: 2.1rem;
  height: 4px;
  padding: 0;
  border: 0;
  border-radius: 2px;
  background: var(--vp-c-divider);
  overflow: hidden;
  cursor: pointer;
}

.topo__step::after {
  content: "";
  position: absolute;
  inset: 0;
  background: var(--pv-mark);
  transform: scaleX(0);
  transform-origin: left;
}

.topo__step.is-on::after {
  transform: scaleX(1);
  transition: transform var(--stepdur, 7s) linear;
}

.topo.is-paused .topo__step.is-on::after { transition: none; transform: scaleX(0.35); }
.topo__step.is-done::after { transform: scaleX(1); background: var(--vp-c-text-3); }

/* ------------------------------------------------------------------ */
/* Sections                                                            */
/* ------------------------------------------------------------------ */

.band {
  padding: 3.75rem 0;
  border-bottom: 1px solid var(--vp-c-divider);
}

.band--last { border-bottom: 0; padding-bottom: 4.5rem; }

.band--story { padding: 4.25rem 0; }

html:not(.dark) .band--story,
html:not(.dark) .band--alt {
  background: var(--vp-c-bg-alt);
}

.home-page h2 {
  margin: 0;
  padding: 0;
  border: 0;
  max-width: 20em;
  font-family: var(--pv-font-display);
  font-size: clamp(1.5rem, 2.8vw, 2.05rem);
  font-weight: 500;
  letter-spacing: -0.02em;
  line-height: 1.2;
}

.home-page h3 {
  margin: 0 0 0.5rem;
  font-family: var(--vp-font-family-base);
  font-size: 1rem;
  font-weight: 600;
  color: var(--vp-c-text-1);
}

.prose {
  max-width: 40em;
  margin-top: 0.9rem;
  color: var(--vp-c-text-2);
}

.prose p { margin: 0; }
.prose p + p { margin-top: 0.75rem; }

.story {
  max-width: none;
  margin: 0;
  font-size: 1.1rem;
  line-height: 1.7;
  color: var(--vp-c-text-1);
}

.product {
  max-width: none;
  margin: 2.5rem 0 0;
  font-size: 1.05rem;
  line-height: 1.7;
  color: var(--vp-c-text-1);
}

.know {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 1.5rem 2.25rem;
  margin-top: 1.25rem;
}

.know p {
  margin: 0;
  color: var(--vp-c-text-2);
  font-size: 0.95rem;
  line-height: 1.65;
}

.note {
  margin: 1.25rem 0 0;
  font-size: 0.88rem;
  line-height: 1.55;
  color: var(--vp-c-text-3);
}

/* ------------------------------------------------------------------ */
/* Try it                                                              */
/* ------------------------------------------------------------------ */

.try {
  margin-top: 2rem;
}

.try__head {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  gap: 0.25rem 1rem;
}

.try__label {
  margin: 0;
  font-size: 0.75rem;
  font-weight: 600;
  color: var(--vp-c-text-1);
}

.try__hint {
  margin: 0;
  font-size: 0.86rem;
  color: var(--vp-c-text-3);
}

.try :deep(.pg-snippet) { margin: 0.6rem 0 0.9rem; }

/* ------------------------------------------------------------------ */
/* SQL tabs                                                            */
/* ------------------------------------------------------------------ */

.tabs {
  margin-top: 2rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--home-radius);
  overflow: hidden;
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
}

.tabs__bar {
  display: flex;
  flex-wrap: wrap;
  gap: 0;
  background: var(--vp-c-bg-soft);
  border-bottom: 1px solid var(--vp-c-border);
}

.tabs__tab {
  appearance: none;
  padding: 0.65rem 1.1rem;
  border: 0;
  border-bottom: 2px solid transparent;
  margin-bottom: -1px;
  border-radius: 0;
  background: transparent;
  color: var(--vp-c-text-2);
  font-family: var(--vp-font-family-mono);
  font-size: 0.8125rem;
  font-weight: 500;
  cursor: pointer;
}

.tabs__tab:hover,
.tabs__tab:focus-visible { color: var(--vp-c-text-1); }

.tabs__tab.is-active {
  color: var(--vp-c-text-1);
  border-bottom-color: var(--vp-button-brand-bg);
}

.tabs__panel {
  display: grid;
  grid-template-columns: minmax(0, 1.12fr) minmax(0, 0.88fr);
  min-height: 17rem;
  background: var(--vp-code-block-bg);
  overflow: hidden;
}

.tabs__code {
  display: flex;
  align-items: flex-start;
  padding: 1rem 1.15rem;
}

.tabs__code :deep(div[class*="language-"]) {
  width: 100%;
  min-width: 0;
  margin: 0;
  border: 0 !important;
  background: transparent;
  box-shadow: none;
}

.tabs__code :deep(pre) {
  padding: 0;
  margin: 0;
  font-size: 0.875rem;
  line-height: 1.55;
}

.tabs__code :deep(code) {
  padding: 0 !important;
  font-size: inherit !important;
  line-height: inherit;
}

.tabs__code :deep(.lang),
.tabs__code :deep(.copy),
.tabs__code :deep(.line-numbers-wrapper) { display: none; }

.tabs__why {
  padding: 1.5rem 1.6rem;
  border-left: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg-elv);
  font-size: 0.92rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
}

.tabs__why p { margin: 0; }

.expect {
  margin-top: 1rem !important;
  padding: 0.5rem 0.85rem;
  border: 1px solid var(--vp-c-border);
  border-left: 0.25em solid var(--vp-c-tip-1);
  border-radius: var(--home-radius);
  background: var(--vp-c-tip-soft);
  font-size: 0.86rem;
  color: var(--vp-c-text-1);
}

.expect b { color: var(--vp-c-tip-1); font-weight: 600; }
.dark .expect b { color: var(--pv-mark); }

.more {
  display: inline-block;
  margin-top: 0.9rem;
  font-size: 0.9rem;
  font-weight: 600;
}

/* ------------------------------------------------------------------ */
/* Why migrate                                                         */
/* ------------------------------------------------------------------ */

.defs {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 1.5rem 2.5rem;
  max-width: none;
  margin: 2rem 0 0;
}

.defs > div {
  padding: 1rem 1.15rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--home-radius);
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
}

.defs dt {
  margin-bottom: 0.3rem;
  font-family: var(--vp-font-family-base);
  font-size: 1rem;
  font-weight: 600;
}

.defs dd {
  margin: 0;
  font-size: 0.92rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
}

.pledges {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 1.75rem;
  margin-top: 3rem;
}

.pledge {
  padding: 1.1rem 1.15rem 1.15rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--home-radius);
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
}

.pledge p {
  margin: 0 0 0.6rem;
  font-size: 0.92rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
}

.pledge a { font-size: 0.9rem; font-weight: 600; }

/* ------------------------------------------------------------------ */
/* Documentation map                                                   */
/* ------------------------------------------------------------------ */

.dir {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 1.25rem;
  margin-top: 1.9rem;
}

.dir__card {
  padding: 1.25rem 1.4rem 1rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--home-radius);
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
}

.dir__card h3 {
  margin-bottom: 0.6rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.68rem;
  font-weight: 500;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--vp-c-text-3);
}

.dir__card a {
  display: block;
  padding: 0.42rem 0;
  color: var(--vp-c-text-1);
  font-size: 0.95rem;
  font-weight: 500;
  line-height: 1.35;
}

.dir__card a + a { border-top: 1px solid var(--vp-c-border-soft); }

.dir__card a:hover,
.dir__card a:focus-visible {
  color: var(--vp-c-brand-1);
  text-decoration: none;
}

.dir__card a small {
  display: block;
  font-size: 0.8rem;
  font-weight: 400;
  color: var(--vp-c-text-3);
}

/* ------------------------------------------------------------------ */
/* Responsive                                                          */
/* ------------------------------------------------------------------ */

@media (max-width: 980px) {
  .hero { padding-top: 2.75rem; }
  .hero__grid { grid-template-columns: minmax(0, 1fr); gap: 2.25rem; }
  .tabs__panel { grid-template-columns: minmax(0, 1fr); min-height: 0; }
  .tabs__why { border-left: 0; border-top: 1px solid var(--vp-c-divider); }
  .pledges, .dir, .know { grid-template-columns: minmax(0, 1fr); gap: 1.25rem; }
  .defs { grid-template-columns: minmax(0, 1fr); gap: 1rem; }
  .core, .univec { grid-template-columns: minmax(0, 1fr); gap: 2rem; }
}

@media (max-width: 720px) {
  .wrap { width: min(100% - 2rem, 72rem); }
  .band { padding: 2.75rem 0; }
  .topo { padding: 1rem 0.75rem 0.75rem; }
  .topo__cap p { font-size: 0.82rem; }
  .tabs__tab { padding: 0.55rem 0.85rem; font-size: 0.78rem; }
  .tabs__code { padding: 0.35rem 0.25rem; }
  .tabs__code :deep(pre) { overflow-x: auto; -webkit-overflow-scrolling: touch; }
  .tabs__why { padding: 1.15rem 1rem; }
  .try :deep(.pg-snippet__code) { flex-basis: auto; }
}
</style>
