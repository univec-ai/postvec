<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import CopyCommand from "./CopyCommand.vue";
import { ARCHES, DISTROS, PG_MAJORS, SITE } from "../site";

const distro = ref<(typeof DISTROS)[number]["id"]>("debian12");
const pg = ref<(typeof PG_MAJORS)[number]>(18);
const arch = ref<(typeof ARCHES)[number]["id"]>("amd64");
const variant = ref<"remote" | "local">("local");

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
  const postvec = SITE.release;
  const ort = `${SITE.onnxRuntimeVersion}-${SITE.packageRelease}`;
  const model = `${SITE.bundledModelVersion}-${SITE.packageRelease}`;
  if (d.family === "deb") {
    const common = [
      `postvec-cli_${postvec}${d.tag}_${a.deb}.deb`,
      `postgresql-${major}-postvec_${postvec}${d.tag}_${a.deb}.deb`,
    ];
    const extras = [
      `postvec-onnxruntime_${ort}${d.tag}_${a.deb}.deb`,
      `postvec-model-minilm-l6-v2_${model}${d.tag}_all.deb`,
      `postvec-extras_${postvec}${d.tag}_all.deb`,
    ];
    return variant.value === "local" ? [...common, ...extras] : common;
  }
  const rpmArch = a.rpm;
  const common = [
    `postvec-cli-${postvec}${d.tag}.${rpmArch}.rpm`,
    `postgresql${major}-postvec-${postvec}${d.tag}.${rpmArch}.rpm`,
  ];
  const extras = [
    `postvec-onnxruntime-${ort}${d.tag}.${rpmArch}.rpm`,
    `postvec-model-minilm-l6-v2-${model}${d.tag}.noarch.rpm`,
    `postvec-extras-${postvec}${d.tag}.noarch.rpm`,
  ];
  return variant.value === "local" ? [...common, ...extras] : common;
});

const releaseBase = computed(
  () => `${SITE.github}/releases/download/${SITE.releaseTag}`
);

const installCmd = computed(() => {
  const names = files.value.map((f) => `./${f}`).join(" \\\n  ");
  const tool = selectedDistro.value.family === "deb" ? "apt" : "dnf";
  return `sudo ${tool} install \\\n  ${names}`;
});

const imageTag = computed(() => {
  const suffix = variant.value === "local" ? "-local" : "-remote";
  return `${SITE.ghcr}:${SITE.release}-pg${pg.value}${suffix}`;
});

const movingTag = computed(() => {
  const suffix = variant.value === "local" ? "-local" : "-remote";
  return `${SITE.ghcr}:pg${pg.value}${suffix}`;
});

// The inference node: one package per distribution and architecture (no
// PostgreSQL major), its own image repository, its own licence.
const serverFiles = computed(() => {
  const d = selectedDistro.value;
  const a = selectedArch.value;
  const postvec = SITE.release;
  return d.family === "deb"
    ? [`postvec-server_${postvec}${d.tag}_${a.deb}.deb`]
    : [`postvec-server-${postvec}${d.tag}.${a.rpm}.rpm`];
});

const serverInstallCmd = computed(() => {
  const d = selectedDistro.value;
  const a = selectedArch.value;
  const tool = d.family === "deb" ? "apt" : "dnf";
  const ort = `${SITE.onnxRuntimeVersion}-${SITE.packageRelease}`;
  const model = `${SITE.bundledModelVersion}-${SITE.packageRelease}`;
  // The node serves from /opt/postvec, where the runtime and model packages
  // install — the same three "extras" files the local payload lists,
  // whichever payload the selector above is showing.
  const extras =
    d.family === "deb"
      ? [
          `postvec-onnxruntime_${ort}${d.tag}_${a.deb}.deb`,
          `postvec-model-minilm-l6-v2_${model}${d.tag}_all.deb`,
          `postvec-extras_${SITE.release}${d.tag}_all.deb`,
        ]
      : [
          `postvec-onnxruntime-${ort}${d.tag}.${a.rpm}.rpm`,
          `postvec-model-minilm-l6-v2-${model}${d.tag}.noarch.rpm`,
          `postvec-extras-${SITE.release}${d.tag}.noarch.rpm`,
        ];
  // The CLI too: the node package only Recommends it, and a local-file
  // install cannot fetch a Recommends that is not supplied.
  const cli =
    d.family === "deb"
      ? `postvec-cli_${SITE.release}${d.tag}_${a.deb}.deb`
      : `postvec-cli-${SITE.release}${d.tag}.${a.rpm}.rpm`;
  const names = [...serverFiles.value, cli, ...extras].map((f) => `./${f}`).join(" \\\n  ");
  // The certificate pair before the unit starts: the packaged service fails
  // closed without one, so a copied command that went straight to
  // `enable --now` would start a node that immediately dies.
  return [
    `sudo ${tool} install \\\n  ${names}`,
    "sudo install -o root -g postvec-server -m 0644 server.crt /etc/postvec-server/server.crt",
    "sudo install -o root -g postvec-server -m 0640 server.key /etc/postvec-server/server.key",
    "sudo systemctl enable --now postvec-server",
  ].join("\n");
});

const serverImageTag = computed(() => `${SITE.ghcrServer}:${SITE.release}`);

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
    const match = data.find((r) => r.tag_name === SITE.releaseTag);
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
    <div class="release-note" v-if="releaseState === 'loading'">
      <span class="release-kicker">Release preview</span>
      <p>
        Checking GitHub for <code>{{ SITE.releaseTag }}</code>. The exact
        artifact names are listed below in the meantime.
      </p>
    </div>

    <div class="release-note" v-else-if="releaseState === 'empty'">
      <span class="release-kicker">Release preview</span>
      <p>
        <code>{{ SITE.releaseTag }}</code> is not published. The exact artifact
        names below are for release rehearsal and local builds.
      </p>
    </div>

    <div class="release-note" v-else-if="releaseState === 'error'">
      <span class="release-kicker">Release status unavailable</span>
      <p>
        GitHub could not be checked. Confirm <code>{{ SITE.releaseTag }}</code>
        on the releases page before using the artifact paths below.
      </p>
    </div>

    <div class="release-note release-note--live" v-else>
      <span class="release-kicker">Published release</span>
      <p>
        <a :href="release?.html_url" target="_blank" rel="noreferrer">
          {{ release?.tag_name }}
        </a>
        includes checksums, attestations, packages and matching image tags.
      </p>
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
          <option value="local">Local (ONNX + MiniLM; both modes)</option>
          <option value="remote">Remote (extension + CLI)</option>
        </select>
      </label>
    </div>

    <h3>Container image</h3>
    <CopyCommand :command="`docker pull ${imageTag}`" label="Pinned tag" />
    <p class="hint">
      The moving tag is <code>{{ movingTag }}</code>. Pin the versioned
      tag in production, preferably by digest. A preview tag may not
      resolve until the release is published. Every PostgreSQL image tag
      includes the major.
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
      Publication path:
      <code>{{ releaseBase }}/</code> plus the filename.
    </p>
    <CopyCommand :command="installCmd" :label="selectedDistro.family === 'deb' ? 'apt' : 'dnf'" />
    <p class="hint">
      Use <code>apt</code> / <code>dnf</code>, not <code>dpkg</code> /
      <code>rpm -i</code>, so PostgreSQL, pgvector and ELF dependencies
      resolve. Packages install files only. They never restart the cluster or
      create a database. Checksums and Sigstore attestations:
      <a href="/docs/install/verify">verify artifacts</a>.
    </p>

    <h3>Inference node (remote mode)</h3>
    <p class="hint">
      <code>postvec-server</code> is the node the remote payload dials. One
      package per distribution and architecture, no PostgreSQL major; the same
      release, the same checksums and attestations. Licensed under
      <strong>{{ SITE.serverLicense }}</strong>. The extension, CLI, runtime,
      model packages and PostgreSQL images stay under the PostgreSQL License.
    </p>
    <ul class="files">
      <li v-for="name in serverFiles" :key="name">
        <a
          v-if="assetUrl(name)"
          :href="assetUrl(name)!"
          rel="noreferrer"
        >{{ name }}</a>
        <code v-else>{{ name }}</code>
      </li>
    </ul>
    <CopyCommand :command="serverInstallCmd" :label="selectedDistro.family === 'deb' ? 'apt' : 'dnf'" />
    <p class="hint">
      The unit reads <code>/opt/postvec</code>, where the runtime and model
      packages install. The package creates the service account and starts
      nothing: the two <code>install</code> lines put your certificate pair
      where the packaged configuration looks (key
      <code>root:postvec-server 0640</code>), and only then is the unit
      started. The CLI and the extras are Recommends of the node package;
      they are listed because a local-file install cannot fetch them on its
      own.
    </p>
    <CopyCommand :command="`docker pull ${serverImageTag}`" label="Node image" />
    <p class="hint">
      The same packages as an image, serving the bundled model. The dashboard
      is on port 22222. The moving tag is
      <code>{{ SITE.ghcrServer }}:latest</code>. gRPC (33333) and discovery
      (22222) are unauthenticated: keep them on a private network.
    </p>
  </div>
</template>

<style scoped>
.dl {
  margin: 1.4rem 0 2rem;
}

.release-note {
  display: grid;
  grid-template-columns: minmax(7rem, 0.28fr) 1fr;
  gap: 1rem;
  padding: 1rem 0;
  border-top: 1px solid var(--vp-c-divider);
  border-bottom: 1px solid var(--vp-c-divider);
  margin-bottom: 1.5rem;
}

.release-note p {
  margin: 0;
  font-size: 0.94rem;
  line-height: 1.55;
}

.release-note--live {
  border-top-color: var(--vp-c-brand-1);
}

.release-kicker {
  font-family: var(--vp-font-family-mono);
  font-size: 0.7rem;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-3);
}

.pickers {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 0;
  border-top: 1px solid var(--vp-c-divider);
  border-bottom: 1px solid var(--vp-c-divider);
  margin-bottom: 2rem;
}

label {
  display: flex;
  flex-direction: column;
  gap: 0.45rem;
  padding: 0.85rem 1rem 0.9rem;
  border-right: 1px solid var(--vp-c-divider);
  border-bottom: 1px solid var(--vp-c-divider);
  font-size: 0.8rem;
  font-weight: 600;
  color: var(--vp-c-text-2);
}

label:nth-child(odd) {
  padding-left: 0;
}

label:nth-child(even) {
  padding-right: 0;
  border-right: 0;
}

label:nth-last-child(-n + 2) {
  border-bottom: 0;
}

select {
  appearance: none;
  border: 0;
  border-bottom: 1px solid var(--vp-c-border);
  background: transparent;
  color: var(--vp-c-text-1);
  border-radius: 0;
  padding: 0.4rem 1.25rem 0.35rem 0;
  font: inherit;
}

h3 {
  margin: 2.25rem 0 0.7rem;
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

@media (max-width: 640px) {
  .release-note {
    grid-template-columns: 1fr;
    gap: 0.45rem;
  }

  .pickers {
    grid-template-columns: 1fr;
  }

  label {
    border-right: 0;
    border-bottom: 1px solid var(--vp-c-divider);
    padding-right: 0;
    padding-left: 0;
  }

  label:nth-last-child(-n + 2) {
    border-bottom: 1px solid var(--vp-c-divider);
  }

  label:last-child {
    border-bottom: 0;
  }
}
</style>
