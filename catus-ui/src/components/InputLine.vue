<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue';
import { useRuntimeStore } from '../stores/runtime';

const store = useRuntimeStore();
const textarea = ref<HTMLTextAreaElement | null>(null);
const candidateList = ref<HTMLUListElement | null>(null);

/**
 * History recall cursor: `null` while editing fresh text; otherwise the
 * index into `inputHistory` currently shown. `draft` stashes the text the
 * user was typing before recalling, restored when paging back down.
 */
const historyIndex = ref<number | null>(null);
const draft = ref('');

const placeholder = computed(() =>
  store.busy
    ? 'working… your message will be processed when the turn ends'
    : 'type a message, /command, or @agent task',
);

onMounted(() => {
  textarea.value?.focus();
});

// Overlays/panels bump the nonce when they close to hand focus back here.
watch(
  () => store.focusInputNonce,
  () => textarea.value?.focus(),
);

// Recompute candidates on every edit (async responses are guarded against
// reordering inside the store).
watch(
  () => store.inputText,
  () => {
    store.recomputeCandidates();
    resize();
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

/** Grow the textarea with its content (capped by CSS max-height). */
async function resize() {
  await nextTick();
  const el = textarea.value;
  if (!el) return;
  el.style.height = 'auto';
  el.style.height = `${el.scrollHeight}px`;
}

function resetHistory() {
  historyIndex.value = null;
  draft.value = '';
}

function recallHistory(delta: number) {
  const history = store.inputHistory;
  if (!history.length) return;
  let index = historyIndex.value;
  if (index === null) {
    if (delta > 0) return;
    draft.value = store.inputText;
    index = history.length - 1;
  } else {
    const next = index + delta;
    if (next >= history.length) {
      store.inputText = draft.value;
      resetHistory();
      return;
    }
    index = Math.max(0, next);
  }
  historyIndex.value = index;
  store.inputText = history[index] ?? '';
}

function onKeydown(e: KeyboardEvent) {
  // IME composition (e.g. Chinese/Japanese input) uses Enter to confirm a
  // candidate; never treat it as "send".
  if (e.isComposing || e.keyCode === 229) return;
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault();
    const text = store.inputText;
    store.inputText = '';
    resetHistory();
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
      recallHistory(-1);
    }
    return;
  }
  if (e.key === 'ArrowDown') {
    if (cycling) {
      e.preventDefault();
      store.cycleCandidate(1);
    } else if (historyIndex.value !== null && !store.inputText.includes('\n')) {
      e.preventDefault();
      recallHistory(1);
    }
  }
}

function onInput() {
  // Any real edit drops the history recall cursor.
  resetHistory();
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
        :placeholder="placeholder"
        @keydown="onKeydown"
        @input="onInput"
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
  /* Long candidate lines wrap instead of overflowing horizontally. */
  white-space: pre-wrap;
  word-break: break-word;
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

@media (max-width: 560px) {
  .row {
    padding: 8px 12px;
  }

  .candidates li {
    padding: 1px 12px;
  }
}
</style>
