<script setup lang="ts">
import { withBase } from "vitepress";
import CopyCommand from "./CopyCommand.vue";
import { SITE } from "../site";

const docker = [
  "docker run -d --name postvec \\",
  "  -e POSTGRES_PASSWORD=demo \\",
  "  -e POSTGRES_DB=app \\",
  "  -p 127.0.0.1:5433:5432 \\",
  `  ${SITE.ghcr}:${SITE.release}-pg18-embedded`,
].join("\n");
</script>

<template>
  <div class="home-page">
    <section class="hero">
      <div class="wrap">
        <p class="eyebrow">PostgreSQL extension</p>
        <h1>postvec</h1>
        <p class="lede">
          Maintains a shadow <code>pgvector</code> column for a text column,
          hybrid search (FTS + vector, RRF) in one call, and conversion of
          stored vectors from one embedding model to another without
          re-embedding the source text. Inference runs in the PostgreSQL
          launcher, or on a ninference node you operate. No provider API keys
          in the database.
        </p>
        <div class="cta">
          <a class="btn btn-primary" :href="withBase('/docs/')">Docs</a>
          <a class="btn btn-ghost" :href="withBase('/download')">Download</a>
        </div>
        <CopyCommand :command="docker" label="docker" />
      </div>
    </section>

    <section class="prose">
      <div class="wrap narrow">
        <h2>Vector lock-in, embedding debt</h2>
        <p>
          An embedding is only meaningful in the space of the model that
          produced it. <code>ada-002</code>, BGE, Gemini, Arctic and the rest
          are incompatible even when the dimensions happen to match. Call that
          <a :href="withBase('/docs/concepts/lock-in')">vector lock-in</a>.
          Once a corpus is in one of those spaces, changing model usually
          means re-embedding everything, rebuilding the index, dual-writing
          through the cutover. That cost is
          <a :href="withBase('/docs/concepts/lock-in')">embedding debt</a>.
        </p>
        <p>
          postvec converts the stored vectors instead
          (<code>migrate()</code>). If you would rather not move them,
          <code>adopt()</code> the existing column and let query embedding
          bridge into that space
          (<a :href="withBase('/docs/guides/bridge')">embed-bridge</a>).
        </p>
      </div>
    </section>

    <section class="prose">
      <div class="wrap narrow">
        <h2>SQL</h2>
        <pre><code>SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2'
);

SELECT d.body, s.rrf_score
  FROM postvec.search(
         'public.docs', 'body',
         'switch models without redoing work',
         filter => '{"category": "engineering"}'::jsonb
       ) s
  JOIN docs d ON d.id = s.pk_value::bigint;

SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3'
);</code></pre>
        <p>
          Vectors fill asynchronously. <code>search()</code> embeds the query
          inline. Filters are typed JSON, applied in both legs before
          <code>LIMIT</code>. There is no <code>rag()</code> / chat function.
        </p>
      </div>
    </section>

    <section class="prose">
      <div class="wrap narrow">
        <h2>Embedded, then remote</h2>
        <p>
          <strong>Embedded</strong> (what the docs start with): one
          InferenceEngine inside the PostgreSQL launcher. Models on local
          disk. Text never leaves the host. No account. This is the on-prem
          shape - no third-party embed API, no outbound inference.
        </p>
        <p>
          <strong>Remote</strong> (<code>grpc</code>): the same SQL against a
          ninference fleet you run. Distributed work, CPU or GPU, the private
          model catalogue, support. Organisation accounts. Still not a SaaS
          embed API; traffic stays on your network.
        </p>
        <p>
          One package. Mode is a GUC plus a restart. RDS / Aurora cannot set
          <code>shared_preload_libraries = 'postvec'</code> and are not
          supported.
        </p>
        <p>
          <a :href="withBase('/docs/concepts/modes')">Modes in detail</a>
        </p>
      </div>
    </section>

    <section class="prose">
      <div class="wrap narrow">
        <h2>Models</h2>
        <p>
          Public registry (anonymous): a subset of open-weight embedders and
          a subset of conversion pairs. <code>postvec model pull</code>, no
          login.
        </p>
        <p>
          Private registry (organisation account): the full embed suite,
          100+ conversion pairs, the better converters.
          <a :href="SITE.univec">univec.ai</a>.
        </p>
        <p>
          Bundled with the embedded image:
          <code>sentence-transformers-all-minilm-l6-v2</code> (384-d).
        </p>
      </div>
    </section>

    <section class="prose last">
      <div class="wrap narrow">
        <h2>Install</h2>
        <ul>
          <li>
            <a :href="withBase('/docs/install/docker')">Docker</a>
            - isolated cluster, nothing installed on the host
          </li>
          <li>
            <a :href="withBase('/docs/install/packages')">Packages</a>
            - apt / dnf, then <code>postvec setup --embedded</code>
          </li>
          <li>
            <a :href="withBase('/docs/install/source')">Source</a>
            - cargo-pgrx; do not mix with package-owned files
          </li>
        </ul>
        <p>
          Extension, CLI and packaging are PostgreSQL-licensed. Converter
          weights are a separate UniVec product.
        </p>
      </div>
    </section>
  </div>
</template>

<style scoped>
.home-page {
  color: var(--vp-c-text-1);
}

.wrap {
  max-width: 1080px;
  margin: 0 auto;
  padding: 0 1.5rem;
}

.wrap.narrow {
  max-width: 40rem;
}

.hero {
  padding: 6rem 0 3.5rem;
  border-bottom: 1px solid var(--vp-c-divider);
}

.eyebrow {
  font-family: "JetBrains Mono", ui-monospace, monospace;
  font-size: 0.74rem;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-3);
  margin: 0 0 0.9rem;
}

.hero h1 {
  font-family: "Manrope", sans-serif;
  font-size: clamp(2.4rem, 5vw, 3.6rem);
  line-height: 1.05;
  letter-spacing: -0.03em;
  font-weight: 800;
  margin: 0;
}

.lede {
  max-width: 38rem;
  margin: 1.2rem 0 1.5rem;
  font-size: 1.05rem;
  line-height: 1.6;
  color: var(--vp-c-text-2);
}

.cta {
  display: flex;
  flex-wrap: wrap;
  gap: 0.7rem;
  margin-bottom: 1.3rem;
}

.btn {
  display: inline-flex;
  align-items: center;
  padding: 0.55rem 1rem;
  border-radius: 8px;
  font-weight: 600;
  font-size: 0.92rem;
  text-decoration: none;
}

.btn-primary {
  background: var(--vp-button-brand-bg);
  color: var(--vp-button-brand-text);
}

.btn-ghost {
  border: 1px solid var(--vp-c-border);
  background: var(--vp-c-bg-elv);
  color: var(--vp-c-text-1);
}

.prose {
  padding: 2.6rem 0;
  border-bottom: 1px solid var(--vp-c-divider);
}

.prose.last {
  border-bottom: 0;
}

.prose h2 {
  font-family: "Manrope", sans-serif;
  font-size: 1.25rem;
  letter-spacing: -0.02em;
  margin: 0 0 0.85rem;
}

.prose p,
.prose li {
  color: var(--vp-c-text-2);
  font-size: 0.98rem;
  line-height: 1.65;
}

.prose p + p {
  margin-top: 0.85rem;
}

.prose a {
  color: var(--vp-c-brand-1);
  text-underline-offset: 0.15em;
}

.prose ul {
  margin: 0.4rem 0 1rem;
  padding-left: 1.2rem;
}

.prose li + li {
  margin-top: 0.35rem;
}

.prose pre {
  margin: 0 0 1rem;
  padding: 0.9rem 1rem;
  border-radius: 8px;
  border: 1px solid var(--vp-c-divider);
  background: var(--vp-code-block-bg);
  color: var(--pv-code-fg);
  overflow-x: auto;
  font-size: 0.78rem;
  line-height: 1.5;
  white-space: pre;
}

@media (max-width: 720px) {
  .hero {
    padding-top: 4.8rem;
  }
}
</style>
