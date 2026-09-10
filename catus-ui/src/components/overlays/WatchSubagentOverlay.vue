<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';

const store = useRuntimeStore();
const messagesEl = ref<HTMLElement | null>(null);
/** Follow new messages unless the user scrolled up. */
const following = ref(true);

const summary = computed(() =>
  store.watching ? store.subagents.find((s) => s.id === store.watching) : undefined,
);

// Subagent events trigger snapshot refreshes; follow them while open.
watch(
  () => store.subagents,
  () => {
    if (store.watching) store.refreshWatch();
  },
  { deep: true },
);

watch(
  () => store.watchingMessages.length,
  async () => {
    if (!following.value) return;
    await nextTick();
    const el = messagesEl.value;
    if (el) el.scrollTop = el.scrollHeight;
  },
);

function onScroll() {
  const el = messagesEl.value;
  if (!el) return;
  following.value = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
}
</script>

<template>
  <div class="panel">
    <div class="head">
      subagent {{ store.watching }}
      <span v-if="summary" class="state">[{{ summary.state }}] {{ summary.name }}</span>
    </div>
    <div v-if="summary" class="task">{{ summary.task }}</div>
    <div v-if="summary?.result" class="result">{{ summary.result }}</div>
    <div v-if="summary?.error" class="error">{{ summary.error }}</div>
    <div ref="messagesEl" class="messages" @scroll.passive="onScroll">
      <div v-for="(message, index) in store.watchingMessages" :key="index" class="line" :class="message.role">
        <span class="role">{{ message.role }}</span>
        <pre>{{ message.content }}</pre>
      </div>
      <div v-if="!store.watchingMessages.length" class="empty">no messages yet</div>
    </div>
    <div class="hint">Esc close</div>
  </div>
</template>

<style scoped>
.panel {
  width: 760px;
  max-width: 92vw;
  height: 80vh;
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
  font-size: 13px;
  display: flex;
  gap: 12px;
  align-items: baseline;
}

.state {
  color: var(--fg-dim);
  font-weight: normal;
  font-size: 12px;
}

.task {
  color: var(--fg-dim);
  font-size: 12px;
  margin: 6px 0 10px;
}

.result {
  border-left: 3px solid var(--ok);
  padding: 6px 10px;
  margin-bottom: 10px;
  white-space: pre-wrap;
}

.error {
  border-left: 3px solid var(--err);
  padding: 6px 10px;
  margin-bottom: 10px;
  color: var(--err);
  white-space: pre-wrap;
}

.messages {
  flex: 1;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.line {
  border-left: 2px solid var(--border);
  padding-left: 10px;
}

.line .role {
  font-size: 10px;
  text-transform: uppercase;
  color: var(--fg-dim);
  letter-spacing: 0.08em;
}

.line.user {
  border-color: var(--accent);
}

.line.assistant {
  border-color: var(--ok);
}

.line.tool,
.line.event {
  color: var(--fg-dim);
  font-size: 13px;
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
