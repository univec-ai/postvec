<script setup lang="ts">
/* The open-core layering, drawn as three stacked bands: PostgreSQL at the
   base, the PostgreSQL-licensed extension on top of it and the optional
   BUSL server above. Used on the home page and on /server. */
defineProps<{ compact?: boolean }>();
</script>

<template>
  <figure class="stack" :class="{ 'stack--compact': compact }" aria-label="postvec open-core layers">
    <div class="stack__layer stack__layer--server">
      <div class="stack__head">
        <span class="stack__name">postvec-server</span>
        <span class="stack__lic">BUSL-1.1 · optional</span>
      </div>
      <ul class="stack__items">
        <li>Managed PostgreSQL</li>
        <li>CPU and GPU fleets</li>
        <li>Dashboard and HTTP API</li>
        <li>Process isolation</li>
      </ul>
    </div>

    <div class="stack__layer stack__layer--ext">
      <div class="stack__head">
        <span class="stack__name">postvec</span>
        <span class="stack__lic">PostgreSQL License · open source</span>
      </div>
      <ul class="stack__items">
        <li>enable, search, adopt, migrate</li>
        <li>Sync worker</li>
        <li>Embedded inference</li>
        <li>External providers</li>
        <li>CLI and packages</li>
      </ul>
    </div>

    <div class="stack__layer stack__layer--pg">
      <span class="stack__name">PostgreSQL 16, 17, 18</span>
      <span class="stack__lic">with pgvector 0.8+</span>
    </div>
  </figure>
</template>

<style scoped>
.stack {
  display: grid;
  gap: 0.55rem;
  margin: 0;
}

.stack__layer {
  position: relative;
  padding: 1rem 1.15rem;
  border: 1px solid var(--vp-c-border);
  border-radius: 6px;
  background: var(--vp-c-bg-elv);
  box-shadow: var(--vp-shadow-1);
}

.stack__layer--server {
  margin: 0 1.5rem;
  border-color: color-mix(in srgb, var(--pv-mark) 55%, var(--vp-c-border));
  background:
    linear-gradient(135deg, color-mix(in srgb, var(--pv-mark) 9%, transparent), transparent 60%),
    var(--vp-c-bg-elv);
  border-style: dashed;
}

.stack__layer--ext {
  border-color: color-mix(in srgb, var(--pv-mark) 70%, var(--vp-c-border));
  box-shadow:
    0 0 0 3px color-mix(in srgb, var(--pv-mark) 10%, transparent),
    var(--vp-shadow-2);
}

.stack__layer--pg {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  justify-content: space-between;
  gap: 0.25rem 1rem;
  padding: 0.7rem 1.15rem;
  background: var(--vp-c-bg-soft);
  box-shadow: none;
}

.stack__head {
  display: flex;
  flex-wrap: wrap;
  align-items: baseline;
  justify-content: space-between;
  gap: 0.25rem 1rem;
}

.stack__name {
  font-family: var(--vp-font-family-mono);
  font-size: 0.92rem;
  font-weight: 500;
  color: var(--vp-c-text-1);
}

.stack__lic {
  font-family: var(--vp-font-family-mono);
  font-size: 0.68rem;
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-3);
}

.stack__layer--ext .stack__lic,
.stack__layer--server .stack__lic {
  color: var(--pv-mark);
}

.stack__items {
  display: flex;
  flex-wrap: wrap;
  gap: 0.4rem;
  margin: 0.75rem 0 0;
  padding: 0;
  list-style: none;
}

.stack__items li {
  margin: 0;
  padding: 0.18rem 0.5rem;
  border: 1px solid var(--vp-c-border);
  border-radius: 4px;
  background: var(--vp-c-bg);
  font-size: 0.8rem;
  line-height: 1.4;
  color: var(--vp-c-text-2);
}

.stack--compact .stack__layer--server { margin: 0 1rem; }

@media (max-width: 720px) {
  .stack__layer--server { margin: 0 0.6rem; }
}
</style>
