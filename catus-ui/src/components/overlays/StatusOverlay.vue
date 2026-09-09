<script setup lang="ts">
import { useRuntimeStore } from '../../stores/runtime';

const store = useRuntimeStore();

function fmt(n: number) {
  return n.toLocaleString();
}
</script>

<template>
  <div class="panel">
    <div class="head">session status</div>
    <div class="grid">
      <span class="k">model</span>
      <span>{{ store.currentModel.id }} ({{ store.currentModel.name || '-' }})</span>
      <span class="k">context window</span>
      <span>{{ store.currentModel.context_window ? fmt(store.currentModel.context_window) + ' tokens' : 'unknown' }}</span>
      <span class="k">session</span>
      <span>{{ store.sessionCwd }}</span>
      <span class="k">requests</span>
      <span>{{ store.requestCount }}</span>
      <span class="k">tokens</span>
      <span>
        prompt {{ fmt(store.usage.prompt_tokens) }} · completion {{ fmt(store.usage.completion_tokens) }}
        · total {{ fmt(store.usage.total_tokens) }} · cached {{ fmt(store.usage.cached_tokens) }}
      </span>
      <span class="k">active skills</span>
      <span>{{ store.activeSkills.length ? store.activeSkills.join(', ') : 'none' }}</span>
      <span class="k">memory</span>
      <span>
        {{ store.memoryAvailable ? (store.memorySessionEnabled ? 'enabled' : 'disabled for session') : 'unavailable' }}
      </span>
      <span class="k">subagents</span>
      <span>
        <template v-if="!store.subagents.length">none</template>
        <template v-else>
          <div v-for="s in store.subagents" :key="s.id">
            {{ s.id }} [{{ s.state }}] {{ s.name }} — {{ s.task }}
          </div>
        </template>
      </span>
      <span class="k">todos</span>
      <span>
        <template v-if="!store.todos.items.length">none</template>
        <template v-else>
          <div v-for="todo in store.todos.items" :key="todo.id">
            [{{ todo.done ? 'x' : ' ' }}] {{ todo.text }}
          </div>
        </template>
      </span>
    </div>
    <div class="hint">Esc close</div>
  </div>
</template>

<style scoped>
.panel {
  width: 640px;
  max-width: 90vw;
  max-height: 80vh;
  overflow-y: auto;
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
  margin-bottom: 12px;
}

.grid {
  display: grid;
  grid-template-columns: 140px 1fr;
  gap: 6px 14px;
}

.k {
  color: var(--fg-dim);
}

.hint {
  margin-top: 12px;
  color: var(--fg-dim);
  font-size: 11px;
}
</style>
