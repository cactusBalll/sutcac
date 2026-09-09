<script setup lang="ts">
import { nextTick, ref, watch } from 'vue';
import { useRuntimeStore } from '../stores/runtime';

const store = useRuntimeStore();
const textarea = ref<HTMLTextAreaElement | null>(null);
const candidateList = ref<HTMLUListElement | null>(null);

// Recompute candidates on every edit (async responses are guarded against
// reordering inside the store).
watch(
  () => store.inputText,
  () => {
    store.recomputeCandidates();
  },
);

// Keep the highlighted candidate visible while cycling.
watch(
  () => store.selectedCandidate,
  async (selected) => {
    if (selected === null || !candidateList.value) return;
    await nextTick();
    const item = candidateList.value.children[selected] as HTMLElement | undefined;
    item?.scrollIntoView({ block: 'nearest' });
  },
);

function onKeydown(e: KeyboardEvent) {
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    const text = store.inputText;
    store.inputText = '';
    store.sendInput(text);
    return;
  }
  if (e.key === 'Tab') {
    e.preventDefault();
    store.cycleCandidate(e.shiftKey ? -1 : 1);
    return;
  }
  if (e.key === 'Escape') {
    store.clearCandidateSelection();
    return;
  }
  // ↑/↓ cycle slash-command candidates; otherwise they recall history.
  const cycling = store.inputText.startsWith('/') && store.candidates.length > 0;
  if (e.key === 'ArrowUp' && (cycling || !store.inputText.includes('\n'))) {
    e.preventDefault();
    if (cycling) {
      store.cycleCandidate(-1);
    } else {
      const last = store.inputHistory[store.inputHistory.length - 1];
      if (last) store.inputText = last;
    }
    return;
  }
  if (e.key === 'ArrowDown' && cycling) {
    e.preventDefault();
    store.cycleCandidate(1);
  }
}

function onCandidateClick(index: number) {
  store.chooseCandidate(index);
  textarea.value?.focus();
}
</script>

<template>
  <div class="input-line">
    <ul
      v-if="store.candidates.length"
      ref="candidateList"
      class="candidates"
    >
      <li
        v-for="(candidate, i) in store.candidates"
        :key="candidate"
        :class="{ selected: store.selectedCandidate === i }"
        @mousedown.prevent="onCandidateClick(i)"
      >
        {{ candidate }}
      </li>
    </ul>
    <div class="row">
      <span class="prompt">›</span>
      <textarea
        ref="textarea"
        v-model="store.inputText"
        rows="1"
        placeholder="type a message, /command, or @agent task"
        :disabled="false"
        @keydown="onKeydown"
      />
    </div>
  </div>
</template>

<style scoped>
.input-line {
  display: flex;
  flex-direction: column;
  background: var(--bg-input);
  border-top: 1px solid var(--border);
}

.candidates {
  list-style: none;
  margin: 0;
  padding: 4px 0;
  max-height: 17rem;
  overflow-y: auto;
  border-bottom: 1px solid var(--border);
  color: var(--accent);
}

.candidates li {
  padding: 1px 20px;
  cursor: pointer;
  white-space: pre;
}

.candidates li.selected {
  color: var(--bg);
  background: var(--accent);
  font-weight: 600;
}

.row {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  padding: 10px 20px;
}

.prompt {
  color: var(--accent);
  padding-top: 8px;
}

textarea {
  flex: 1;
  font: inherit;
  color: var(--fg);
  background: transparent;
  border: none;
  resize: none;
  outline: none;
  max-height: 30vh;
}
</style>
