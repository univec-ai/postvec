<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import CopyCommand from "./CopyCommand.vue";
import { ARCHES, DISTROS, PG_MAJORS, SITE } from "../site";

const distro = ref<(typeof DISTROS)[number]["id"]>("debian12");
const pg = ref<(typeof PG_MAJORS)[number]>(18);
const arch = ref<(typeof ARCHES)[number]["id"]>("amd64");
const variant = ref<"remote" | "embedded">("embedded");

const selectedDistro = computed(
  () => DISTROS.find((d) => d.id === distro.value) ?? DISTROS[0]
);
const selectedArch = computed(
  () => ARCHES.find((a) => a.id === arch.value) ?? ARCHES[0]
);

const files = computed(() => {
  const d = selectedDistro.value;
  const a = selectedArch.value;
  const major = pg.value;
  const rel = SITE.release;
  if (d.family === "deb") {
    const common = [
      `postvec-cli_${rel}${d.tag}_${a.deb}.deb`,
      `postgresql-${major}-postvec_${rel}${d.tag}_${a.deb}.deb`,
    ];
    const embedded = [
      `postvec-onnxruntime_${rel}${d.tag}_${a.deb}.deb`,
      `postvec-model-minilm-l6-v2_${rel}${d.tag}_all.deb`,
      `postvec-embedded_${rel}${d.tag}_all.deb`,
    ];
    return variant.value === "embedded" ? [...common, ...embedded] : common;
  }
  const rpmArch = a.rpm;
  const common = [
    `postvec-cli-${rel}${d.tag}.${rpmArch}.rpm`,
    `postgresql${major}-postvec-${rel}${d.tag}.${rpmArch}.rpm`,
  ];
  const embedded = [
    `postvec-onnxruntime-${rel}${d.tag}.${rpmArch}.rpm`,
    `postvec-model-minilm-l6-v2-${rel}${d.tag}.noarch.rpm`,
    `postvec-embedded-${rel}${d.tag}.noarch.rpm`,
  ];
  return variant.value === "embedded" ? [...common, ...embedded] : common;
});

const releaseBase = computed(
  () => `${SITE.github}/releases/download/${SITE.releaseTag}`
);

const installCmd = computed(() => {
  const names = files.value.map((f) => `./${f}`).join(" \\\n  ");
  const tool = selectedDistro.value.family === "deb" ? "apt" : "dnf";
  return `sudo ${tool} install \\\n  ${names}`;
});

const verifyCmd = computed(() => {
  const first = files.value.find((n) => n.includes("postvec") && !n.includes("cli")) ?? files.value[0];
  return [
    "sha256sum --ignore-missing --check SHA256SUMS",
    `gh attestation verify ./${first} \\`,
    `  --repo ${SITE.githubRepo} \\`,
    `  --signer-workflow ${SITE.signerWorkflow}`,
  ].join("\n");
});

const imageTag = computed(() => {
  const suffix = variant.value === "embedded" ? "-embedded" : "";
  return `${SITE.ghcr}:${SITE.release}-pg${pg.value}${suffix}`;
});

const movingTag = computed(() => {
  const suffix = variant.value === "embedded" ? "-embedded" : "";
  return `${SITE.ghcr}:pg${pg.value}${suffix}`;
});

type ReleaseAsset = { name: string; browser_download_url: string; size: number };
type ReleaseInfo = {
  tag_name: string;
  html_url: string;
  published_at: string;
  assets: ReleaseAsset[];
};

const release = ref<ReleaseInfo | null>(null);
const releaseState = ref<"loading" | "ok" | "empty" | "error">("loading");

onMounted(async () => {
  try {
    const res = await fetch(
      `https://api.github.com/repos/${SITE.githubRepo}/releases?per_page=20`
    );
    if (!res.ok) {
      releaseState.value = "error";
      return;
    }
    const data = (await res.json()) as ReleaseInfo[];
    const match =
      data.find((r) => r.tag_name === SITE.releaseTag) ??
      data.find((r) => r.tag_name.startsWith("postvec-v"));
    if (!match) {
      releaseState.value = "empty";
      return;
    }
    release.value = match;
    releaseState.value = "ok";
  } catch {
    releaseState.value = "error";
  }
});

function assetUrl(name: string): string | null {
  const found = release.value?.assets.find((a) => a.name === name);
  return found?.browser_download_url ?? null;
}
</script>

<template>
  <div class="dl">
    <div class="notice" v-if="releaseState !== 'ok'">
      <strong>Hosting is GitHub Releases + GHCR.</strong>
      There is no signed apt/yum repository yet. When a
      <code>{{ SITE.releaseTag }}</code> release is published, the links below
      light up automatically. Until then, pull the image or install the files
      you built locally.
    </div>

    <div class="notice ok" v-else>
      Live release
      <a :href="release?.html_url" target="_blank" rel="noreferrer">
        {{ release?.tag_name }}
      </a>
      — checksums and attestations ship next to the packages.
    </div>

    <div class="pickers">
      <label>
        Distribution
        <select v-model="distro">
          <option v-for="d in DISTROS" :key="d.id" :value="d.id">
            {{ d.label }}
          </option>
        </select>
      </label>
      <label>
        PostgreSQL
        <select v-model.number="pg">
          <option v-for="m in PG_MAJORS" :key="m" :value="m">{{ m }}</option>
        </select>
      </label>
      <label>
        Architecture
        <select v-model="arch">
          <option v-for="a in ARCHES" :key="a.id" :value="a.id">
            {{ a.label }}
          </option>
        </select>
      </label>
      <label>
        Payload
        <select v-model="variant">
          <option value="embedded">Embedded (engine + MiniLM)</option>
          <option value="remote">Remote only (extension + CLI)</option>
        </select>
      </label>
    </div>

    <h3>Container</h3>
    <CopyCommand :command="`docker pull ${imageTag}`" label="Pinned tag" />
    <p class="hint">
      Moving tag <code>{{ movingTag }}</code> exists too. There is no
      <code>latest</code> — it would hide the PostgreSQL major. Pin the
      versioned tag in production, preferably by digest.
    </p>
    <p class="hint">
      PostgreSQL 18 volumes mount at <code>/var/lib/postgresql</code>. 16 and 17
      use <code>/var/lib/postgresql/data</code>. Never change major by retagging
      an existing volume.
    </p>

    <h3>Packages</h3>
    <ul class="files">
      <li v-for="name in files" :key="name">
        <a
          v-if="assetUrl(name)"
          :href="assetUrl(name)!"
          rel="noreferrer"
        >{{ name }}</a>
        <code v-else>{{ name }}</code>
      </li>
    </ul>
    <p class="hint" v-if="releaseState !== 'ok'">
      Expected release URL:
      <code>{{ releaseBase }}/</code> plus the filename.
    </p>
    <CopyCommand :command="installCmd" :label="selectedDistro.family === 'deb' ? 'apt' : 'dnf'" />
    <p class="hint">
      Use <code>apt</code> / <code>dnf</code>, not <code>dpkg</code> /
      <code>rpm -i</code>, so PostgreSQL, pgvector, and ELF dependencies
      resolve. Packages install files only — they never restart the cluster or
      create a database.
    </p>

    <h3>Verify</h3>
    <CopyCommand :command="verifyCmd" label="Checksums + Sigstore" />
  </div>
</template>

<style scoped>
.dl {
  margin: 1.4rem 0 2rem;
}

.notice {
  padding: 0.85rem 1rem;
  border-radius: 10px;
  border: 1px solid var(--vp-c-divider);
  background: var(--vp-c-bg-soft);
  font-size: 0.94rem;
  line-height: 1.5;
  margin-bottom: 1.2rem;
}

.notice.ok {
  border-color: var(--vp-c-brand-1);
}

.pickers {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: 0.75rem;
  margin-bottom: 1.5rem;
}

label {
  display: flex;
  flex-direction: column;
  gap: 0.35rem;
  font-size: 0.8rem;
  font-weight: 600;
  color: var(--vp-c-text-2);
}

select {
  appearance: none;
  border: 1px solid var(--vp-c-border);
  background: var(--vp-c-bg-elv);
  color: var(--vp-c-text-1);
  border-radius: 8px;
  padding: 0.5rem 0.7rem;
  font: inherit;
}

h3 {
  margin: 1.5rem 0 0.7rem;
  font-family: "Manrope", sans-serif;
}

.files {
  margin: 0 0 0.9rem;
  padding-left: 1.1rem;
  font-family: "JetBrains Mono", ui-monospace, monospace;
  font-size: 0.82rem;
}

.files em {
  font-style: normal;
  color: var(--vp-c-text-3);
  font-size: 0.75rem;
}

.hint {
  color: var(--vp-c-text-3);
  font-size: 0.9rem;
  line-height: 1.5;
  margin: 0.7rem 0 0;
}

.dl > .copy-cmd + .hint {
  margin-top: 0.7rem;
}
</style>
