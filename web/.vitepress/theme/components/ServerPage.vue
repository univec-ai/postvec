<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from "vue";
import { withBase } from "vitepress";
import { SITE } from "../site";
import OpenCoreStack from "./OpenCoreStack.vue";
import PgSnippet from "./PgSnippet.vue";

/* ------------------------------------------------------------------ */
/* Hero figure: SMIL dots on fixed paths, paused out of view, on a     */
/* hidden tab and replaced by a static drawing for reduced motion.     */
/* ------------------------------------------------------------------ */
const animate = ref(true);
const paused = ref(false);
const figure = ref<HTMLElement | null>(null);
const svg = ref<SVGSVGElement | null>(null);
let observer: IntersectionObserver | undefined;
let inView = true;

function syncRunning() {
  const run = animate.value && inView && !document.hidden;
  paused.value = !run;
  if (run) svg.value?.unpauseAnimations();
  else svg.value?.pauseAnimations();
}

onMounted(() => {
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    animate.value = false;
    return;
  }
  if ("IntersectionObserver" in window && figure.value) {
    observer = new IntersectionObserver(
      (entries) => {
        inView = entries[0]?.isIntersecting ?? true;
        syncRunning();
      },
      { threshold: 0.1 },
    );
    observer.observe(figure.value);
  }
  document.addEventListener("visibilitychange", syncRunning);
  syncRunning();
});

onBeforeUnmount(() => {
  observer?.disconnect();
  document.removeEventListener("visibilitychange", syncRunning);
});

/* ------------------------------------------------------------------ */
/* Content                                                             */
/* ------------------------------------------------------------------ */
const UNIVEC = SITE.univec;

type Feature = { icon: string; title: string; body: string; href: string; link: string };

const features: Feature[] = [
  {
    icon: "M4 7c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3Zm0 0v10c0 1.7 3.6 3 8 3s8-1.3 8-3V7M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3",
    title: "Managed PostgreSQL",
    body:
      "On Amazon RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon, postvec-server installs postvec as a plain SQL schema, runs the sync worker against the database and answers search(text) through a wire-protocol proxy.",
    href: "/docs/server/managed",
    link: "Managed PostgreSQL",
  },
  {
    icon: "M5 5h14v14H5zM9 9h6v6H9zM12 2v3M12 19v3M2 12h3M19 12h3",
    title: "Process isolation",
    body:
      "Inference runs in its own process, with its own limits and restart cycle. A native fault in a model stops that node, and the PostgreSQL service continues to run.",
    href: "/docs/server/usage",
    link: "When to use it",
  },
  {
    icon: "M5 6h6v5H5zM13 6h6v5h-6zM9 14h6v5H9zM8 11v3M16 11v3",
    title: "Throughput and fleets",
    body:
      "One node runs multi-threaded next to PostgreSQL. More nodes on CPU or GPU hosts find each other by gossip, postvec spreads requests across them and one fleet serves several databases.",
    href: "/docs/server/fleet",
    link: "Fleet",
  },
  {
    icon: "M3 5h18v12H3zM8 21h8M12 17v4M7 9h4M7 12h7",
    title: "Remote model management",
    body:
      "The dashboard and the HTTP API list loaded models, send test requests and pull, activate or remove models from the UniVec registries, from a browser that reaches the node.",
    href: "/docs/server/dashboard",
    link: "Dashboard",
  },
  {
    icon: "M3 19h18M6 16l4-5 3 3 5-7",
    title: "Health and metrics",
    body:
      "Readiness routes and metrics report request latency, fleet membership, models on disk against models loaded and the queue depth of each managed database.",
    href: "/docs/server/reference#metrics",
    link: "Reference",
  },
];

const hosts = ["Amazon RDS", "Aurora", "Cloud SQL", "Azure Flexible Server", "Supabase", "Neon"];

const installCmd = `postvec-server managed install \\
  --dsn 'postgresql://postvec_worker@db.example/app?sslmode=verify-full' \\
  --password-file /etc/postvec-server/database.pw`;

const configJson = `"managed": [{
  "name": "prod",
  "dsn": "postgresql://postvec_worker@db.example/app?sslmode=verify-full",
  "password_file": "/etc/postvec-server/database.pw",
  "sync": true,
  "proxy_port": 5433
}]`;

const searchSql = `-- through the proxy port (5433)
SELECT * FROM postvec.search(
  'public.docs', 'body', 'reset password',
  limit_n => 5
);`;

type Plan = {
  name: string;
  price: string;
  note: string;
  featured?: boolean;
  items: string[];
  cta: { text: string; href: string; external?: boolean };
  more?: { text: string; href: string };
};

const plans: Plan[] = [
  {
    name: "Free",
    price: "€0",
    note: "Individuals and organizations",
    items: [
      "The extension, CLI and packages, for any use",
      "postvec-server for development, testing, CI and staging",
      "postvec-server for personal, noncommercial production use",
      "One 30-day production evaluation per organization",
      "Private converters for noncommercial use",
    ],
    cta: { text: "Install the server", href: "/docs/server/node" },
  },
  {
    name: "postvec pro",
    price: "€30",
    note: "Per organization, per month, excluding tax",
    featured: true,
    items: [
      "Production use of postvec-server by the organization",
      "Unlimited nodes and environments for internal use",
      "Commercial use of the private converter catalogue",
      "€30 of UniVec API credit every month, for hosted embed and convert calls or UniVec as a provider",
      "Best-effort support",
    ],
    cta: { text: "Subscribe on univec.ai", href: `${UNIVEC}/dashboard/postvec`, external: true },
    more: { text: "Plan details", href: `${UNIVEC}/postvec#pro` },
  },
  {
    name: "Enterprise",
    price: "Contract",
    note: "Annual agreement",
    items: [
      "Support with a service-level agreement",
      "Air-gapped model catalogue",
      "Custom conversion pairs",
      "Redistribution, OEM and hosting postvec-server for third parties",
    ],
    cta: { text: "Contact UniVec", href: `${UNIVEC}/contact`, external: true },
  },
];
</script>

<template>
  <main class="sp">
    <!-- ============================================================ -->
    <!-- Hero                                                          -->
    <!-- ============================================================ -->
    <header class="sp-hero">
      <div class="sp-wrap sp-hero__grid">
        <div class="sp-hero__copy">
          <p class="sp-kicker">
            <span class="sp-tag">BUSL-1.1</span>
            Optional server for the open-source extension
          </p>
          <h1>postvec-server</h1>
          <p class="sp-sub">
            A companion inference server for postvec. It runs models in a
            separate, multi-threaded process, scales out to a fleet of CPU and
            GPU nodes and brings postvec to managed PostgreSQL services. The
            SQL and the sync behaviour are the same as with embedded inference.
          </p>
          <div class="sp-cta">
            <a class="sp-btn sp-btn--go" :href="withBase('/docs/server/node')">Install the server</a>
            <a class="sp-btn" href="#plans">Plans</a>
          </div>
          <p class="sp-fine">
            Free for development, testing, CI and personal use, with one
            30-day production evaluation per organization.
          </p>
        </div>

        <figure ref="figure" class="sp-topo" :class="{ 'is-static': !animate, 'is-paused': paused }">
          <svg
            ref="svg"
            viewBox="24 24 610 310"
            xmlns="http://www.w3.org/2000/svg"
            role="img"
            aria-labelledby="sp-topo-title sp-topo-desc"
          >
            <title id="sp-topo-title">postvec-server between databases and models</title>
            <desc id="sp-topo-desc">
              A self-hosted PostgreSQL cluster calls a fleet of three
              postvec-server nodes over gRPC. Two managed databases are served
              by the server's worker over SQL. The nodes form a gossip group,
              one of them is a GPU node, and a dashboard manages them.
            </desc>

            <!-- wires -->
            <path id="sp-w1" class="w" d="M 168 70 C 250 70, 280 70, 356 70" />
            <path id="sp-w2" class="w w--sql" d="M 168 180 C 250 180, 280 180, 356 180" />
            <path id="sp-w3" class="w w--sql" d="M 168 290 C 250 290, 280 290, 356 290" />
            <path class="w w--mesh" d="M 420 100 L 420 150 M 420 210 L 420 260" />
            <path class="w w--ui" d="M 484 180 L 516 180" />
            <text class="t-wire" x="262" y="60" text-anchor="middle">gRPC</text>
            <text class="t-wire" x="262" y="170" text-anchor="middle">worker · SQL</text>
            <text class="t-wire" x="262" y="280" text-anchor="middle">worker · SQL</text>
            <text class="t-wire t-accent" x="408" y="128" text-anchor="end">gossip</text>

            <!-- databases -->
            <g class="db">
              <path d="M 40 52 a 64 14 0 0 1 128 0 v 38 a 64 14 0 0 1 -128 0 z" />
              <ellipse cx="104" cy="52" rx="64" ry="14" class="db__lid" />
              <text class="t-name" x="104" y="84" text-anchor="middle">PostgreSQL</text>
              <text class="t-sub" x="104" y="98" text-anchor="middle">self-hosted</text>
            </g>
            <g class="db db--managed">
              <path d="M 40 162 a 64 14 0 0 1 128 0 v 38 a 64 14 0 0 1 -128 0 z" />
              <ellipse cx="104" cy="162" rx="64" ry="14" class="db__lid" />
              <text class="t-name" x="104" y="194" text-anchor="middle">Amazon RDS</text>
              <text class="t-sub" x="104" y="208" text-anchor="middle">managed schema</text>
            </g>
            <g class="db db--managed">
              <path d="M 40 272 a 64 14 0 0 1 128 0 v 38 a 64 14 0 0 1 -128 0 z" />
              <ellipse cx="104" cy="272" rx="64" ry="14" class="db__lid" />
              <text class="t-name" x="104" y="304" text-anchor="middle">Cloud SQL</text>
              <text class="t-sub" x="104" y="318" text-anchor="middle">managed schema</text>
            </g>

            <!-- nodes -->
            <g class="node">
              <rect x="356" y="44" width="128" height="52" rx="6" />
              <circle cx="370" cy="58" r="3" class="led" />
              <text class="t-node" x="428" y="70" text-anchor="middle">postvec-server</text>
              <text class="t-sub" x="428" y="85" text-anchor="middle">CPU node</text>
            </g>
            <g class="node">
              <rect x="356" y="154" width="128" height="52" rx="6" />
              <circle cx="370" cy="168" r="3" class="led" />
              <text class="t-node" x="428" y="180" text-anchor="middle">postvec-server</text>
              <text class="t-sub" x="428" y="195" text-anchor="middle">CPU node</text>
            </g>
            <g class="node node--gpu">
              <rect x="356" y="264" width="128" height="52" rx="6" />
              <circle cx="370" cy="278" r="3" class="led" />
              <text class="t-node" x="428" y="290" text-anchor="middle">postvec-server</text>
              <text class="t-sub" x="428" y="305" text-anchor="middle">GPU build</text>
            </g>

            <!-- dashboard -->
            <g class="ui">
              <rect x="516" y="132" width="104" height="96" rx="6" />
              <rect x="516" y="132" width="104" height="18" rx="6" class="ui__bar" />
              <circle cx="527" cy="141" r="2.2" /><circle cx="535" cy="141" r="2.2" /><circle cx="543" cy="141" r="2.2" />
              <rect x="526" y="160" width="84" height="8" rx="2" class="ui__row ui__row--on" />
              <rect x="526" y="174" width="64" height="8" rx="2" class="ui__row" />
              <rect x="526" y="188" width="74" height="8" rx="2" class="ui__row" />
              <text class="t-sub" x="568" y="216" text-anchor="middle">dashboard</text>
            </g>

            <!-- traveling dots -->
            <g v-if="animate" class="dots">
              <circle class="dot dot--in" r="3.2" opacity="0">
                <animateMotion dur="1.5s" begin="0s" repeatCount="indefinite"><mpath href="#sp-w1" /></animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.5s" begin="0s" repeatCount="indefinite" />
              </circle>
              <circle class="dot dot--out" r="3.2" opacity="0">
                <animateMotion dur="1.5s" begin="1.6s" repeatCount="indefinite" keyPoints="1;0" keyTimes="0;1" calcMode="linear"><mpath href="#sp-w1" /></animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.5s" begin="1.6s" repeatCount="indefinite" />
              </circle>
              <circle class="dot dot--in" r="3.2" opacity="0">
                <animateMotion dur="1.5s" begin="0.8s" repeatCount="indefinite"><mpath href="#sp-w2" /></animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.5s" begin="0.8s" repeatCount="indefinite" />
              </circle>
              <circle class="dot dot--out" r="3.2" opacity="0">
                <animateMotion dur="1.5s" begin="2.4s" repeatCount="indefinite" keyPoints="1;0" keyTimes="0;1" calcMode="linear"><mpath href="#sp-w2" /></animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.5s" begin="2.4s" repeatCount="indefinite" />
              </circle>
              <circle class="dot dot--in" r="3.2" opacity="0">
                <animateMotion dur="1.5s" begin="1.4s" repeatCount="indefinite"><mpath href="#sp-w3" /></animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.5s" begin="1.4s" repeatCount="indefinite" />
              </circle>
              <circle class="dot dot--out" r="3.2" opacity="0">
                <animateMotion dur="1.5s" begin="3s" repeatCount="indefinite" keyPoints="1;0" keyTimes="0;1" calcMode="linear"><mpath href="#sp-w3" /></animateMotion>
                <animate attributeName="opacity" values="0;1;1;0" keyTimes="0;0.12;0.88;1" dur="1.5s" begin="3s" repeatCount="indefinite" />
              </circle>
            </g>
          </svg>
          <figcaption>
            <span><i class="key key--in" /> text to the model</span>
            <span><i class="key key--out" /> vectors back</span>
          </figcaption>
        </figure>
      </div>
    </header>

    <!-- ============================================================ -->
    <!-- Open core                                                     -->
    <!-- ============================================================ -->
    <section class="sp-band sp-band--alt" aria-labelledby="sp-core">
      <div class="sp-wrap sp-split">
        <div>
          <p class="sp-eyebrow">Open core</p>
          <h2 id="sp-core">An open-source extension, with the server as an add-on</h2>
          <div class="sp-prose">
            <p>
              The postvec extension, its CLI, the inference engine and the
              packages are released under the PostgreSQL License. In embedded
              mode they cover the complete SQL surface: enable, search, adopt and
              migrate, with local models or external providers.
            </p>
            <p>
              postvec-server adds a second host for inference: process isolation,
              throughput beyond one database host, and managed PostgreSQL. It is
              source-available under the Business Source License 1.1, and each
              version changes to the PostgreSQL License four years after its
              release.
            </p>
          </div>
          <a class="sp-more" :href="withBase('/docs/concepts/modes')">Embedded vs remote</a>
        </div>
        <OpenCoreStack />
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- Features                                                      -->
    <!-- ============================================================ -->
    <section class="sp-band" aria-labelledby="sp-adds">
      <div class="sp-wrap">
        <p class="sp-eyebrow">Capabilities</p>
        <h2 id="sp-adds">What the server adds</h2>
        <div class="sp-features">
          <article
            v-for="(f, i) in features"
            :key="f.title"
            class="sp-feature"
            :class="{ 'sp-feature--lead': i === 0 }"
          >
            <svg class="sp-icon" viewBox="0 0 24 24" aria-hidden="true">
              <path :d="f.icon" />
            </svg>
            <h3>{{ f.title }}</h3>
            <p>{{ f.body }}</p>
            <a :href="withBase(f.href)">{{ f.link }}</a>
          </article>
        </div>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- Managed PostgreSQL                                            -->
    <!-- ============================================================ -->
    <section class="sp-band sp-band--alt" aria-labelledby="sp-managed">
      <div class="sp-wrap">
        <p class="sp-eyebrow">Managed PostgreSQL</p>
        <h2 id="sp-managed">postvec on managed PostgreSQL</h2>
        <p class="sp-lead">
          The database needs pgvector 0.8 or newer and a role with
          <code>CREATE</code> on the application database. The server holds the
          models and provider keys, runs the worker and serves search.
        </p>
        <ul class="sp-hosts">
          <li v-for="h in hosts" :key="h">{{ h }}</li>
        </ul>

        <ol class="sp-steps">
          <li>
            <div class="sp-step__text">
              <h3>Install the schema</h3>
              <p>
                Creates the <code>postvec</code> schema as a plain SQL install.
                The install is transactional, and it runs again after a server
                upgrade to update the schema in place.
              </p>
            </div>
            <pre class="sp-code"><code>{{ installCmd }}</code></pre>
          </li>
          <li>
            <div class="sp-step__text">
              <h3>Add the database to the server</h3>
              <p>
                A <code>managed</code> entry starts the sync worker. Several
                nodes can list the same database; they elect one leader.
                <code>proxy_port</code> opens the search proxy.
              </p>
            </div>
            <pre class="sp-code"><code>{{ configJson }}</code></pre>
          </li>
          <li>
            <div class="sp-step__text">
              <h3>Enable a column and search</h3>
              <p>
                <code>enable()</code> and <code>adopt()</code> work as on a
                self-hosted cluster. The proxy embeds the query text and passes
                every other byte to the database unchanged.
              </p>
            </div>
            <pre class="sp-code"><code>{{ searchSql }}</code></pre>
          </li>
        </ol>
        <a class="sp-more" :href="withBase('/docs/server/managed')">Managed PostgreSQL guide</a>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- Plans                                                         -->
    <!-- ============================================================ -->
    <section id="plans" class="sp-band" aria-labelledby="sp-plans">
      <div class="sp-wrap">
        <p class="sp-eyebrow">Plans</p>
        <h2 id="sp-plans">A subscription covers production use of the server</h2>
        <p class="sp-lead">
          Payment applies to production use of postvec-server or of the private
          converters by an organization. Hobby projects, study, evaluation and
          every non-production environment are free, including inside a company.
          Compliance is contractual. The license travels with each package and
          image.
        </p>

        <div class="sp-plans">
          <article
            v-for="p in plans"
            :key="p.name"
            class="sp-plan"
            :class="{ 'sp-plan--featured': p.featured }"
          >
            <p class="sp-plan__name">{{ p.name }}</p>
            <p class="sp-plan__price">{{ p.price }}</p>
            <p class="sp-plan__note">{{ p.note }}</p>
            <ul>
              <li v-for="item in p.items" :key="item">{{ item }}</li>
            </ul>
            <div class="sp-plan__cta">
              <a
                class="sp-btn"
                :class="{ 'sp-btn--go': p.featured }"
                :href="p.cta.external ? p.cta.href : withBase(p.cta.href)"
                :target="p.cta.external ? '_blank' : undefined"
                :rel="p.cta.external ? 'noopener' : undefined"
              >{{ p.cta.text }}</a>
              <a v-if="p.more" class="sp-plan__more" :href="p.more.href" target="_blank" rel="noopener">{{ p.more.text }}</a>
            </div>
          </article>
        </div>
        <p class="sp-note">
          After a cancellation, production use of the server and the private
          converters ends after a 30-day transition. Vectors already written stay
          in the database, and embedded mode with public models continues to
          work. Full terms: <a :href="withBase('/docs/license')">License</a>.
        </p>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- UniVec                                                        -->
    <!-- ============================================================ -->
    <section class="sp-band sp-band--univec" aria-labelledby="sp-univec">
      <div class="sp-wrap">
        <p class="sp-eyebrow">UniVec</p>
        <h2 id="sp-univec">Built and supported by UniVec</h2>
        <p class="sp-lead">
          UniVec builds embedding translation infrastructure in Dublin, Ireland.
          The same team maintains the extension, postvec-server, the model
          registries and the converters that <code>adopt()</code> and
          <code>migrate()</code> use.
        </p>
        <div class="sp-univec">
          <div>
            <h3>Converter catalogue</h3>
            <p>
              {{ SITE.conversionPairs }} conversion pairs between embedding model
              spaces. A public subset is open to everyone, and
              verified UniVec accounts get the full catalogue.
            </p>
            <a :href="withBase('/docs/models/login')">Login and the private catalogue</a>
          </div>
          <div>
            <h3>Hosted API</h3>
            <p>
              Embedding and vector conversion over REST and MCP, at €0.01 per
              million tokens and €2 per million vectors. postvec reaches it as
              the <code>univec</code> external provider.
            </p>
            <a :href="`${UNIVEC}/pricing`" target="_blank" rel="noopener">Pricing on univec.ai</a>
          </div>
          <div>
            <h3>Enterprise</h3>
            <p>
              Support agreements with response times, an air-gapped catalogue,
              custom conversion pairs, redistribution and OEM terms.
            </p>
            <a :href="`${UNIVEC}/contact`" target="_blank" rel="noopener">Contact UniVec</a>
          </div>
        </div>
      </div>
    </section>

    <!-- ============================================================ -->
    <!-- Install                                                       -->
    <!-- ============================================================ -->
    <section class="sp-band sp-band--last" aria-labelledby="sp-install">
      <div class="sp-wrap">
        <p class="sp-eyebrow">Install</p>
        <h2 id="sp-install">Run a node</h2>
        <p class="sp-lead">
          The image starts with the bundled MiniLM model loaded and generates a
          self-signed certificate. Packages add a systemd unit and a
          configuration file; a source build adds GPU support.
        </p>
        <PgSnippet id="docker-server" />
        <div class="sp-paths">
          <a :href="withBase('/docs/server/docker')"><b>Docker</b><span>Image with MiniLM and a generated certificate</span></a>
          <a :href="withBase('/docs/server/packages')"><b>Packages</b><span>.deb and .rpm with a systemd unit</span></a>
          <a :href="withBase('/docs/server/source')"><b>From source</b><span>CUDA or TensorRT builds</span></a>
          <a :href="withBase('/docs/quickstart-remote')"><b>Quick start remote</b><span>PostgreSQL and a node, two containers</span></a>
        </div>
      </div>
    </section>
  </main>
</template>

<style scoped>
.sp {
  --sp-r: 6px;
  color: var(--vp-c-text-1);
  line-height: 1.5;
}

.sp-wrap { width: min(72rem, calc(100% - 3.5rem)); margin: 0 auto; }

.sp a { color: var(--vp-c-brand-1); text-decoration: none; }
.sp a:hover, .sp a:focus-visible { text-decoration: underline; text-underline-offset: 0.12em; }

.sp code {
  font-family: var(--vp-font-family-mono);
  font-size: 0.85em;
}

.sp p code, .sp li code {
  padding: 0.15em 0.35em;
  border-radius: 4px;
  background: var(--vp-code-bg);
}

/* hero ------------------------------------------------------------- */

.sp-hero {
  position: relative;
  padding: 4rem 0 3.75rem;
  border-bottom: 1px solid var(--vp-c-divider);
  overflow: hidden;
}

.sp-hero::before {
  content: "";
  position: absolute;
  inset: 0;
  background-image:
    linear-gradient(var(--vp-c-border-soft) 1px, transparent 1px),
    linear-gradient(90deg, var(--vp-c-border-soft) 1px, transparent 1px);
  background-size: 28px 28px;
  mask-image: radial-gradient(70% 80% at 70% 40%, #000 20%, transparent 75%);
  pointer-events: none;
}

.sp-hero::after {
  content: "";
  position: absolute;
  width: 34rem;
  height: 34rem;
  right: -6rem;
  top: -10rem;
  background: radial-gradient(circle, color-mix(in srgb, var(--pv-mark) 14%, transparent), transparent 65%);
  pointer-events: none;
}

.sp-hero__grid {
  position: relative;
  z-index: 1;
  display: grid;
  grid-template-columns: minmax(0, 0.95fr) minmax(0, 1.05fr);
  gap: 3rem;
  align-items: center;
}

.sp-kicker {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.6rem;
  margin: 0 0 1.1rem;
  font-size: 0.86rem;
  color: var(--vp-c-text-2);
}

.sp-tag {
  padding: 0.12rem 0.45rem;
  border: 1px solid color-mix(in srgb, var(--pv-mark) 45%, var(--vp-c-border));
  border-radius: 4px;
  background: var(--vp-c-brand-soft);
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  color: var(--pv-mark);
}

.sp h1 {
  margin: 0;
  font-family: var(--vp-font-family-mono);
  font-size: clamp(2.2rem, 4.6vw, 3.3rem);
  font-weight: 500;
  letter-spacing: -0.03em;
  line-height: 1.05;
}

.sp-sub {
  max-width: 34em;
  margin: 1.25rem 0 0;
  font-size: 1.08rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
}

.sp-cta { display: flex; flex-wrap: wrap; gap: 0.7rem; margin-top: 1.7rem; }

.sp-btn {
  display: inline-block;
  padding: 0.45rem 1rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--sp-r);
  background: var(--vp-c-bg-elv);
  color: var(--vp-c-text-1) !important;
  font-size: 0.875rem;
  font-weight: 500;
  line-height: 1.4;
  box-shadow: var(--vp-shadow-1);
}

.sp-btn:hover, .sp-btn:focus-visible {
  text-decoration: none !important;
  background: var(--vp-c-bg-soft);
  border-color: var(--vp-c-text-3);
}

.sp-btn--go {
  background: var(--vp-button-brand-bg);
  border-color: var(--vp-button-brand-border);
  color: var(--vp-button-brand-text) !important;
}

.sp-btn--go:hover, .sp-btn--go:focus-visible {
  background: var(--vp-button-brand-hover-bg);
  border-color: var(--vp-button-brand-hover-bg);
}

.sp-fine { margin: 1.1rem 0 0; font-size: 0.82rem; color: var(--vp-c-text-3); }

/* hero figure */

.sp-topo {
  margin: 0;
  padding: 1.25rem 1.25rem 0.85rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--sp-r);
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-3);
}

.sp-topo svg { display: block; width: 100%; height: auto; }

.sp-topo .w { fill: none; stroke: var(--vp-c-text-3); stroke-width: 1.5; stroke-dasharray: 6 6; animation: sp-flow 1.2s linear infinite; }
.sp-topo .w--sql { stroke: var(--pv-field); stroke-opacity: 0.7; }
.sp-topo .w--mesh { stroke: var(--pv-mark); stroke-dasharray: 3 6; animation-duration: 1.6s; }
.sp-topo .w--ui { stroke-dasharray: 2 4; }

.sp-topo .db path { fill: var(--vp-c-bg-elv); stroke: var(--vp-c-text-1); stroke-width: 1.6; }
.sp-topo .db__lid { fill: var(--vp-c-bg-soft); stroke: var(--vp-c-text-1); stroke-width: 1.6; }
.sp-topo .db--managed path,
.sp-topo .db--managed .db__lid { stroke: var(--pv-field); stroke-dasharray: 4 3; }

.sp-topo .node rect { fill: var(--vp-c-bg-elv); stroke: var(--pv-field); stroke-width: 1.6; }
.sp-topo .node--gpu rect { fill: var(--vp-c-brand-soft); }
.sp-topo .led { fill: var(--pv-mark); animation: sp-led 2.4s ease-in-out infinite; }
.sp-topo .node:nth-of-type(2) .led { animation-delay: 0.8s; }

.sp-topo .ui rect { fill: var(--vp-c-bg); stroke: var(--vp-c-border); }
.sp-topo .ui .ui__bar { fill: var(--vp-c-bg-soft); }
.sp-topo .ui circle { fill: var(--vp-c-text-3); }
.sp-topo .ui__row { fill: var(--vp-c-bg-soft) !important; stroke: none !important; }
.sp-topo .ui__row--on { fill: var(--vp-c-brand-soft) !important; }

.sp-topo text { font-family: var(--vp-font-family-base); }
.sp-topo .t-name { font-size: 13px; font-weight: 600; fill: var(--vp-c-text-1); }
.sp-topo .t-node { font-family: var(--vp-font-family-mono); font-size: 11.5px; font-weight: 500; fill: var(--vp-c-text-1); }
.sp-topo .t-sub { font-size: 10.5px; fill: var(--vp-c-text-3); }
.sp-topo .t-wire { font-family: var(--vp-font-family-mono); font-size: 10.5px; fill: var(--vp-c-text-3); }
.sp-topo .t-accent { fill: var(--pv-mark); }

.sp-topo .dot--in { fill: var(--pv-field); }
.sp-topo .dot--out { fill: var(--pv-mark); }

.sp-topo figcaption {
  display: flex;
  justify-content: center;
  gap: 1.5rem;
  margin-top: 0.4rem;
  padding-top: 0.6rem;
  border-top: 1px solid var(--vp-c-divider);
  font-size: 0.8rem;
  color: var(--vp-c-text-3);
}

.key { display: inline-block; width: 8px; height: 8px; margin-right: 0.35rem; border-radius: 50%; vertical-align: 0; }
.key--in { background: var(--pv-field); }
.key--out { background: var(--pv-mark); box-shadow: 0 0 0 2px var(--vp-c-brand-soft); }

.sp-topo.is-paused .w, .sp-topo.is-paused .led { animation-play-state: paused; }
.sp-topo.is-static .w, .sp-topo.is-static .led { animation: none; }

@keyframes sp-flow { to { stroke-dashoffset: -12; } }
@keyframes sp-led { 0%, 100% { opacity: 1; } 50% { opacity: 0.35; } }

/* bands ------------------------------------------------------------ */

.sp-band { padding: 4rem 0; border-bottom: 1px solid var(--vp-c-divider); }
#plans { scroll-margin-top: calc(var(--vp-nav-height) + 16px); }
.sp-band--last { border-bottom: 0; padding-bottom: 4.5rem; }
html:not(.dark) .sp-band--alt { background: var(--vp-c-bg-alt); }
.dark .sp-band--alt { background: var(--vp-c-bg-alt); }

.sp-band--univec {
  background:
    radial-gradient(60% 90% at 0% 0%, color-mix(in srgb, var(--pv-mark) 8%, transparent), transparent 70%),
    var(--vp-c-bg);
}

.sp-eyebrow {
  margin: 0 0 0.6rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  letter-spacing: 0.12em;
  text-transform: uppercase;
  color: var(--pv-mark);
}

.sp h2 {
  margin: 0;
  padding: 0;
  border: 0;
  max-width: 24em;
  font-family: var(--pv-font-display);
  font-size: clamp(1.5rem, 2.8vw, 2.05rem);
  font-weight: 500;
  letter-spacing: -0.02em;
  line-height: 1.2;
}

.sp h3 {
  margin: 0 0 0.4rem;
  font-size: 1rem;
  font-weight: 600;
  color: var(--vp-c-text-1);
}

.sp-lead { max-width: 44em; margin: 0.9rem 0 0; color: var(--vp-c-text-2); line-height: 1.65; }

.sp-prose { margin-top: 1rem; color: var(--vp-c-text-2); line-height: 1.65; }
.sp-prose p { margin: 0; }
.sp-prose p + p { margin-top: 0.8rem; }

.sp-more { display: inline-block; margin-top: 1.1rem; font-size: 0.92rem; font-weight: 600; }
.sp-more::after { content: " \2192"; }

.sp-note { max-width: 50em; margin: 1.5rem 0 0; font-size: 0.88rem; line-height: 1.6; color: var(--vp-c-text-3); }

.sp-split {
  display: grid;
  grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
  gap: 3rem;
  align-items: center;
}

/* features */

.sp-features {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 1rem;
  margin-top: 2rem;
}

.sp-feature {
  display: flex;
  flex-direction: column;
  padding: 1.25rem 1.3rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--sp-r);
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
  transition: border-color 0.2s, transform 0.2s, box-shadow 0.2s;
}

.sp-feature:hover {
  border-color: color-mix(in srgb, var(--pv-mark) 45%, var(--vp-c-border));
  box-shadow: var(--vp-shadow-2);
  transform: translateY(-2px);
}

.sp-feature--lead {
  grid-column: span 2;
  background:
    linear-gradient(160deg, color-mix(in srgb, var(--pv-mark) 10%, transparent), transparent 55%),
    var(--vp-c-bg-elv);
}

.sp-feature--lead h3 { font-size: 1.15rem; }
.sp-feature--lead p { font-size: 0.98rem; }

.sp-feature p { flex: 1; margin: 0 0 0.8rem; font-size: 0.92rem; line-height: 1.6; color: var(--vp-c-text-2); }
.sp-feature a { font-size: 0.88rem; font-weight: 600; }

.sp-icon {
  width: 34px;
  height: 34px;
  margin-bottom: 0.85rem;
  padding: 6px;
  border-radius: 6px;
  background: var(--vp-c-brand-soft);
  fill: none;
  stroke: var(--pv-mark);
  stroke-width: 1.6;
  stroke-linecap: round;
  stroke-linejoin: round;
}

/* managed */

.sp-hosts { display: flex; flex-wrap: wrap; gap: 0.45rem; margin: 1.25rem 0 0; padding: 0; list-style: none; }
.sp-hosts li {
  margin: 0;
  padding: 0.25rem 0.6rem;
  border: 1px solid var(--vp-c-border);
  border-radius: 4px;
  background: var(--vp-c-bg-elv);
  font-size: 0.84rem;
  color: var(--vp-c-text-1);
}

.sp-steps { counter-reset: step; display: grid; gap: 1rem; margin: 2rem 0 0; padding: 0; list-style: none; }

.sp-steps > li {
  counter-increment: step;
  display: grid;
  grid-template-columns: minmax(0, 0.8fr) minmax(0, 1.2fr);
  gap: 1.5rem;
  align-items: start;
  margin: 0;
  padding: 1.25rem 1.3rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--sp-r);
  background: var(--vp-c-bg-elv);
}

.sp-step__text h3::before {
  content: counter(step, decimal-leading-zero);
  margin-right: 0.6rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.8rem;
  font-weight: 500;
  color: var(--pv-mark);
}

.sp-step__text p { margin: 0; font-size: 0.92rem; line-height: 1.6; color: var(--vp-c-text-2); }

.sp-code {
  margin: 0;
  padding: 0.85rem 1rem;
  overflow-x: auto;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--sp-r);
  background: var(--vp-code-block-bg);
  font-size: 0.8rem;
  line-height: 1.6;
}

.sp-code code { font-size: inherit !important; padding: 0 !important; background: none !important; color: var(--vp-c-text-1); }

/* plans */

.sp-plans {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 1rem;
  margin-top: 2rem;
  align-items: stretch;
}

.sp-plan {
  display: flex;
  flex-direction: column;
  padding: 1.4rem 1.4rem 1.3rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--sp-r);
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
}

.sp-plan--featured {
  position: relative;
  border-color: color-mix(in srgb, var(--pv-mark) 60%, var(--vp-c-border));
  box-shadow: 0 0 0 3px var(--vp-c-brand-soft), var(--vp-shadow-3);
}

.sp-plan--featured::before {
  content: "";
  position: absolute;
  inset: -1px -1px auto;
  height: 3px;
  border-radius: var(--sp-r) var(--sp-r) 0 0;
  background: var(--pv-mark);
}

.sp-plan__name { margin: 0; font-family: var(--vp-font-family-mono); font-size: 0.82rem; color: var(--pv-mark); }
.sp-plan__price { margin: 0.5rem 0 0; font-family: var(--pv-font-display); font-size: 2.2rem; line-height: 1.1; }
.sp-plan__note { margin: 0.2rem 0 0; font-size: 0.84rem; color: var(--vp-c-text-3); }

.sp-plan ul { flex: 1; margin: 1.1rem 0 0; padding: 1rem 0 0; border-top: 1px solid var(--vp-c-divider); list-style: none; }

.sp-plan li {
  position: relative;
  margin: 0 0 0.55rem;
  padding-left: 1.4rem;
  font-size: 0.9rem;
  line-height: 1.5;
  color: var(--vp-c-text-2);
}

.sp-plan li::before {
  content: "";
  position: absolute;
  left: 0.15rem;
  top: 0.4rem;
  width: 0.55rem;
  height: 0.3rem;
  border-left: 1.6px solid var(--pv-mark);
  border-bottom: 1.6px solid var(--pv-mark);
  transform: rotate(-45deg);
}

.sp-plan__cta { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem 1rem; margin-top: 1.1rem; }
.sp-plan__more { font-size: 0.86rem; }

/* univec */

.sp-univec {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 2rem;
  margin-top: 2rem;
  padding-top: 1.75rem;
  border-top: 1px solid var(--vp-c-divider);
}

.sp-univec p { margin: 0 0 0.7rem; font-size: 0.92rem; line-height: 1.6; color: var(--vp-c-text-2); }
.sp-univec a { font-size: 0.88rem; font-weight: 600; }

/* install */

.sp-band--last :deep(.pg-snippet) { margin: 1.5rem 0 0; }

.sp-paths {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 0.75rem;
  margin-top: 1.25rem;
}

.sp-paths a {
  display: block;
  padding: 0.85rem 1rem;
  border: 1px solid var(--vp-c-border);
  border-radius: var(--sp-r);
  background: var(--vp-c-bg-elv);
  color: var(--vp-c-text-1);
  transition: border-color 0.2s;
}

.sp-paths a:hover { text-decoration: none; border-color: var(--pv-mark); }
.sp-paths b { display: block; font-size: 0.95rem; font-weight: 600; }
.sp-paths span { display: block; margin-top: 0.15rem; font-size: 0.82rem; color: var(--vp-c-text-3); }

/* responsive ------------------------------------------------------- */

@media (max-width: 980px) {
  .sp-hero__grid, .sp-split { grid-template-columns: minmax(0, 1fr); gap: 2.25rem; }
  .sp-features, .sp-plans, .sp-univec { grid-template-columns: minmax(0, 1fr); }
  .sp-feature--lead { grid-column: auto; }
  .sp-steps > li { grid-template-columns: minmax(0, 1fr); gap: 0.85rem; }
  .sp-paths { grid-template-columns: repeat(2, minmax(0, 1fr)); }
}

@media (max-width: 720px) {
  .sp-wrap { width: min(100% - 2rem, 72rem); }
  .sp-hero { padding: 2.75rem 0; }
  .sp-band { padding: 2.75rem 0; }
  .sp-topo { padding: 0.75rem 0.5rem 0.6rem; }
  .sp-paths { grid-template-columns: minmax(0, 1fr); }
}

@media (prefers-reduced-motion: reduce) {
  .sp-feature { transition: none; }
  .sp-feature:hover { transform: none; }
}
</style>
