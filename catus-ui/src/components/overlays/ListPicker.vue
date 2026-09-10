<script setup lang="ts">
import { nextTick, onMounted, ref } from 'vue';
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
const root = ref<HTMLElement | null>(null);
const list = ref<HTMLElement | null>(null);

onMounted(async () => {
  await nextTick();
  root.value?.focus();
  scrollCursorIntoView();
});

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

function scrollCursorIntoView() {
  const el = list.value?.children[cursor.value] as HTMLElement | undefined;
  el?.scrollIntoView({ block: 'nearest' });
}

function move(delta: number) {
  if (!props.items.length) return;
  const len = props.items.length;
  cursor.value = (((cursor.value + delta) % len) + len) % len;
  scrollCursorIntoView();
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'ArrowUp') {
    e.preventDefault();
    move(-1);
  } else if (e.key === 'ArrowDown') {
    e.preventDefault();
    move(1);
  } else if (e.key === 'Enter') {
    e.preventDefault();
    const item = props.items[cursor.value];
    if (item !== undefined) pick(item);
  }
}
</script>

<template>
  <div ref="root" class="picker panel" tabindex="-1" @keydown="onKeydown">
    <div class="head">{{ title }}</div>
    <div ref="list" class="items">
      <button
        v-for="(item, index) in items"
        :key="item"
        class="item"
        :class="{ cursor: index === cursor }"
        @click="pick(item)"
        @mouseenter="cursor = index"
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
  outline: none;
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
  color: var(--on-accent);
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
