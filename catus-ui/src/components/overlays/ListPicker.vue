<script setup lang="ts">
import { ref } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';

const props = defineProps<{
  title: string;
  items: string[];
  /** Core action executed on pick (or `prefill_agent` for input prefill). */
  action: string;
  /** Optional preselected entry (`selected` in ShowModels). */
  selected?: number;
}>();

const store = useRuntimeStore();
const cursor = ref(props.selected ?? 0);

function pick(item: string) {
  if (props.action === 'prefill_agent') {
    // Mirrors the TUI: choosing an agent prefills `/agent use <name> `.
    store.inputText = `/agent use ${item} `;
    store.closeOverlay();
    return;
  }
  store.closeOverlay();
  store.pick(props.action, item);
}
</script>

<template>
  <div class="picker panel">
    <div class="head">{{ title }}</div>
    <div class="items">
      <button
        v-for="(item, index) in items"
        :key="item"
        class="item"
        :class="{ cursor: index === cursor }"
        @click="pick(item)"
        @mousemove="cursor = index"
      >
        <span class="marker">{{ index === cursor ? '›' : ' ' }}</span>
        {{ item }}
      </button>
    </div>
    <div v-if="!items.length" class="empty">no entries</div>
    <div class="hint">↑↓ move · Enter choose · Esc cancel</div>
  </div>
</template>

<style scoped>
.panel {
  width: 520px;
  max-width: 90vw;
  max-height: 70vh;
  display: flex;
  flex-direction: column;
  background: var(--bg-elev);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 14px 18px;
}

.head {
  color: var(--accent);
  font-weight: bold;
  text-transform: uppercase;
  font-size: 12px;
  letter-spacing: 0.08em;
  margin-bottom: 10px;
}

.items {
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.item {
  text-align: left;
  background: none;
  border: none;
  padding: 5px 8px;
  border-radius: 4px;
  white-space: pre-wrap;
  word-break: break-word;
}

.item.cursor {
  background: var(--accent-dim);
  color: #fff;
}

.marker {
  color: var(--accent);
  display: inline-block;
  width: 14px;
}

.empty {
  color: var(--fg-dim);
  padding: 10px 0;
}

.hint {
  margin-top: 10px;
  color: var(--fg-dim);
  font-size: 11px;
}
</style>
