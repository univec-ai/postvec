<script setup lang="ts">
import { ref } from "vue";

const props = defineProps<{
  command: string;
  label?: string;
}>();

const copied = ref(false);

async function copy() {
  try {
    await navigator.clipboard.writeText(props.command);
    copied.value = true;
    window.setTimeout(() => {
      copied.value = false;
    }, 1600);
  } catch {
    copied.value = false;
  }
}
</script>

<template>
  <div class="copy-cmd">
    <div v-if="label" class="copy-cmd__label">{{ label }}</div>
    <div class="copy-cmd__row">
      <pre class="copy-cmd__text">{{ command }}</pre>
      <button type="button" class="copy-cmd__btn" @click="copy">
        {{ copied ? "Copied" : "Copy" }}
      </button>
    </div>
  </div>
</template>

<style scoped>
.copy-cmd {
  margin: 0.2rem 0 0.8rem;
}

.copy-cmd__label {
  font-family: "JetBrains Mono", ui-monospace, monospace;
  font-size: 0.68rem;
  letter-spacing: 0.1em;
  text-transform: uppercase;
  color: var(--vp-c-text-3);
  margin-bottom: 0.4rem;
}

.copy-cmd__row {
  display: flex;
  align-items: flex-start;
  gap: 0.75rem;
  padding: 0.85rem 0.95rem;
  border-radius: 10px;
  border: 1px solid var(--vp-c-divider);
  background: var(--vp-code-block-bg);
  color: var(--pv-code-fg);
}

.copy-cmd__text {
  flex: 1;
  margin: 0;
  padding: 0;
  background: transparent;
  border: 0;
  font-family: "JetBrains Mono", ui-monospace, monospace;
  font-size: 0.8rem;
  line-height: 1.5;
  white-space: pre-wrap;
  word-break: break-word;
  color: var(--pv-code-fg);
  overflow: visible;
}

.copy-cmd__btn {
  flex: none;
  border: 1px solid var(--pv-code-btn-border);
  background: var(--pv-code-btn-bg);
  color: var(--pv-code-btn-fg);
  border-radius: 7px;
  padding: 0.28rem 0.6rem;
  font-size: 0.75rem;
  cursor: pointer;
}

.copy-cmd__btn:hover {
  background: var(--vp-code-copy-code-hover-bg, var(--vp-c-bg-alt));
}
</style>
