<script setup lang="ts">
import { computed } from 'vue';
import { useRuntimeStore } from '../stores/runtime';

const store = useRuntimeStore();

const statusClass = computed(() => store.status);

const ctxTokens = computed(() => store.usage.prompt_tokens.toLocaleString());
const totalTokens = computed(() => store.usage.total_tokens.toLocaleString());
</script>

<template>
  <div class="status-bar">
    <div class="left">
      <span class="dot" :class="statusClass" />
      <button class="model" title="switch model" @click="store.openModelPicker()">
        {{ store.currentModel.id || store.currentModel.name }}
      </button>
      <span v-if="store.subagents.length" class="dim">· {{ store.subagents.length }} subagents</span>
      <span v-if="store.memoryAvailable" class="dim">· memory {{ store.memorySessionEnabled ? 'on' : 'off' }}</span>
      <span v-if="store.statusMessage" class="message" :class="{ error: store.status === 'error' }">
        {{ store.statusMessage }}
      </span>
    </div>
    <div class="right">
      <button class="action" title="config editor" @click="store.openConfigPage()">
        config
      </button>
      <span>ctx {{ ctxTokens }} tok | total {{ totalTokens }}</span>
    </div>
  </div>
</template>

<style scoped>
.status-bar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 16px;
  padding: 5px 20px;
  font-size: 12px;
  color: var(--fg-dim);
  background: var(--bg-elev);
  border-top: 1px solid var(--border);
}

.left {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}

.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--fg-dim);
  flex-shrink: 0;
}

.dot.streaming,
.dot.running_tool {
  background: var(--ok);
  animation: pulse 1.2s infinite;
}

.dot.error {
  background: var(--err);
}

@keyframes pulse {
  50% {
    opacity: 0.3;
  }
}

.model {
  color: var(--fg);
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  cursor: pointer;
}

.model:hover {
  color: var(--accent);
  text-decoration: underline;
}

.message {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--accent);
}

.message.error {
  color: var(--err);
}

.dim {
  color: var(--fg-dim);
}

.action {
  font: inherit;
  color: var(--fg-dim);
  background: none;
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 1px 8px;
  cursor: pointer;
}

.action:hover {
  color: var(--accent);
  border-color: var(--accent);
}
</style>
