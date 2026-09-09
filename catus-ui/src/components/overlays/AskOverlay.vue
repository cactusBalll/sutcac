<script setup lang="ts">
import { computed, ref } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';
import type { AskAnswer, AskQuestion } from '../../types';

const props = defineProps<{ questions: AskQuestion[] }>();

const store = useRuntimeStore();

const index = ref(0);
const checked = ref<Set<string>>(new Set());
const otherText = ref('');
/** Answers collected for earlier questions in the same call. */
const collected = ref<AskAnswer[]>([]);

const question = computed(() => props.questions[index.value]);
const progress = computed(() => `${index.value + 1}/${props.questions.length}`);
const isLast = computed(() => index.value + 1 >= props.questions.length);

function currentAnswer(labels: string[]): AskAnswer {
  return {
    prompt: question.value.prompt,
    answer: question.value.multiSelect ? labels : labels[0],
  };
}

function choose(label: string) {
  if (question.value.multiSelect) {
    const next = new Set(checked.value);
    if (next.has(label)) next.delete(label);
    else next.add(label);
    checked.value = next;
  } else {
    advance([label]);
  }
}

/** Record the answer and move on; submit everything on the last question. */
function advance(labels: string[]) {
  collected.value.push(currentAnswer(labels));
  if (isLast.value) {
    const answers = collected.value;
    collected.value = [];
    store.answerInteraction(answers);
    return;
  }
  checked.value = new Set();
  otherText.value = '';
  index.value += 1;
}

function confirmMulti() {
  const labels = [...checked.value];
  if (otherText.value.trim()) labels.push(otherText.value.trim());
  if (labels.length === 0) return;
  advance(labels);
}

function confirmOther() {
  if (!otherText.value.trim()) return;
  advance([otherText.value.trim()]);
}
</script>

<template>
  <div class="ask panel">
    <div class="head">
      <span class="title">{{ question.title }}</span>
      <span class="progress">{{ progress }}</span>
    </div>
    <div class="prompt-text">{{ question.prompt }}</div>
    <div class="options">
      <button
        v-for="option in question.options"
        :key="option.label"
        class="option"
        :class="{ checked: question.multiSelect && checked.has(option.label) }"
        @click="choose(option.label)"
      >
        <span class="check">{{ question.multiSelect ? (checked.has(option.label) ? '[x]' : '[ ]') : '' }}</span>
        <span class="option-label">{{ option.label }}</span>
        <span v-if="option.description" class="option-desc">{{ option.description }}</span>
      </button>
      <div class="other">
        <input
          v-model="otherText"
          type="text"
          placeholder="Other…"
          @keydown.enter.stop="confirmOther"
        />
        <button v-if="question.multiSelect" class="primary" @click="confirmMulti">
          {{ isLast ? 'confirm' : 'next' }}
        </button>
      </div>
    </div>
    <div class="hint">Esc cancel</div>
  </div>
</template>

<style scoped>
.panel {
  width: 560px;
  max-width: 90vw;
  max-height: 80vh;
  overflow-y: auto;
  background: var(--bg-elev);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 16px 20px;
}

.head {
  display: flex;
  justify-content: space-between;
  margin-bottom: 6px;
}

.title {
  color: var(--accent);
  font-weight: bold;
  text-transform: uppercase;
  font-size: 12px;
  letter-spacing: 0.08em;
}

.progress {
  color: var(--fg-dim);
  font-size: 12px;
}

.prompt-text {
  margin-bottom: 14px;
}

.options {
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.option {
  display: flex;
  align-items: baseline;
  gap: 10px;
  text-align: left;
  padding: 8px 12px;
}

.option.checked {
  border-color: var(--accent);
  background: var(--accent-dim);
}

.check {
  color: var(--accent);
}

.option-desc {
  color: var(--fg-dim);
  font-size: 12px;
}

.other {
  display: flex;
  gap: 8px;
  margin-top: 6px;
}

.hint {
  margin-top: 12px;
  color: var(--fg-dim);
  font-size: 11px;
}
</style>
