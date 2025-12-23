<script setup lang="ts">
import { computed, ref } from "vue";
import { PG_MAJORS, usePgMajor, usePkgFamily, type PkgFamily } from "../composables/pgMajor";
import { SNIPPETS, tokens } from "../snippets";

const props = defineProps<{
  id: string;
  /** Optional caption under the command, shown as muted text. */
  caption?: string;
}>();

const pg = usePgMajor();
const family = usePkgFamily();
const copied = ref(false);

const def = computed(() => {
  const found = SNIPPETS[props.id];
  if (!found) {
    throw new Error(`Unknown snippet id: ${props.id}`);
  }
  return found;
});

const t = computed(() => tokens(pg.value));

const activeFamily = computed(() => {
  const families = def.value.families;
  if (!families || families.length === 0) return null;
  return families.find((f) => f.id === family.value) ?? families[0];
});

const command = computed(() => {
  if (activeFamily.value) return activeFamily.value.render(t.value);
  if (def.value.render) return def.value.render(t.value);
  return "";
});

async function copy() {
  try {
    await navigator.clipboard.writeText(command.value);
    copied.value = true;
    window.setTimeout(() => {
      copied.value = false;
    }, 1600);
  } catch {
    copied.value = false;
  }
}

function setFamily(id: PkgFamily) {
  family.value = id;
}
</script>

<template>
  <div class="pg-snippet">
    <div class="pg-snippet__bar">
      <div class="pg-snippet__tabs" role="tablist" aria-label="PostgreSQL major">
        <button
          v-for="major in PG_MAJORS"
          :key="major"
          type="button"
          role="tab"
          class="pg-snippet__tab"
          :class="{ 'is-active': pg === major }"
          :aria-selected="pg === major"
          @click="pg = major"
        >
          PG {{ major }}
        </button>
      </div>
      <div
        v-if="def.families"
        class="pg-snippet__tabs pg-snippet__tabs--family"
        role="tablist"
        aria-label="Package family"
      >
        <button
          v-for="item in def.families"
          :key="item.id"
          type="button"
          role="tab"
          class="pg-snippet__tab"
          :class="{ 'is-active': activeFamily?.id === item.id }"
          :aria-selected="activeFamily?.id === item.id"
          @click="setFamily(item.id)"
        >
          {{ item.label }}
        </button>
      </div>
    </div>
    <div class="pg-snippet__body">
      <pre class="pg-snippet__code"><code>{{ command }}</code></pre>
      <button type="button" class="pg-snippet__copy" @click="copy">
        {{ copied ? "Copied" : "Copy" }}
      </button>
    </div>
    <p v-if="caption" class="pg-snippet__caption">{{ caption }}</p>
  </div>
</template>

<style scoped>
.pg-snippet {
  margin: 1rem 0 1.25rem;
}

.pg-snippet__bar {
  display: flex;
  flex-wrap: wrap;
  align-items: flex-end;
  justify-content: space-between;
  gap: 0.4rem 1rem;
  border: 1px solid var(--vp-c-divider);
  border-bottom: 0;
  background: var(--vp-c-bg-soft);
}

.pg-snippet__tabs {
  display: flex;
  flex-wrap: wrap;
  gap: 0;
}

.pg-snippet__tab {
  appearance: none;
  border: 0;
  border-bottom: 2px solid transparent;
  background: transparent;
  color: var(--vp-c-text-3);
  padding: 0.5rem 0.75rem;
  font-family: var(--vp-font-family-mono);
  font-size: 0.72rem;
  letter-spacing: 0.04em;
  cursor: pointer;
}

.pg-snippet__tab.is-active {
  color: var(--vp-c-brand-1);
  border-bottom-color: var(--vp-c-brand-1);
  font-weight: 500;
}

.pg-snippet__tab:hover,
.pg-snippet__tab:focus-visible {
  color: var(--vp-c-text-1);
}

.pg-snippet__body {
  display: flex;
  align-items: flex-start;
  flex-wrap: wrap;
  gap: 0.75rem;
  padding: 0.85rem 0.95rem;
  border: 1px solid var(--vp-c-divider);
  background: var(--vp-code-block-bg);
  color: var(--pv-code-fg);
}

.pg-snippet__code {
  flex: 1 1 16rem;
  min-width: 0;
  margin: 0;
  padding: 0;
  background: transparent;
  border: 0;
  font-family: var(--vp-font-family-mono);
  font-size: 0.8rem;
  line-height: 1.5;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  color: var(--pv-code-fg);
}

.pg-snippet__copy {
  flex: none;
  border: 1px solid var(--pv-code-btn-border);
  background: var(--pv-code-btn-bg);
  color: var(--pv-code-btn-fg);
  border-radius: 0;
  padding: 0.28rem 0.6rem;
  font-size: 0.75rem;
  cursor: pointer;
}

.pg-snippet__copy:hover {
  background: var(--vp-code-copy-code-hover-bg, var(--vp-c-bg-alt));
}

.pg-snippet__caption {
  margin: 0.45rem 0 0;
  font-size: 0.86rem;
  line-height: 1.5;
  color: var(--vp-c-text-3);
}

@media (max-width: 640px) {
  .pg-snippet__bar {
    flex-direction: column;
    align-items: stretch;
  }

  .pg-snippet__body {
    flex-direction: column;
  }

  .pg-snippet__copy {
    align-self: flex-end;
  }
}
</style>
