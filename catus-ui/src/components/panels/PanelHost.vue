<script setup lang="ts">
import { onMounted, onUnmounted } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';
import SessionsPanel from './SessionsPanel.vue';
import SkillsPanel from './SkillsPanel.vue';
import McpPanel from './McpPanel.vue';
import AgentsPanel from './AgentsPanel.vue';

const store = useRuntimeStore();

const titles = {
  sessions: 'session history',
  skills: 'Agent Skills',
  mcp: 'MCP servers',
  agents: 'Agents',
} as const;

function onKeydown(e: KeyboardEvent) {
  if (store.panel && e.key === 'Escape') {
    e.preventDefault();
    store.closePanel();
  }
}

onMounted(() => window.addEventListener('keydown', onKeydown));
onUnmounted(() => window.removeEventListener('keydown', onKeydown));
</script>

<template>
  <div v-if="store.panel" class="panel-page">
    <div class="head">
      <span class="title">{{ titles[store.panel] }}</span>
      <button class="close" title="close (Esc)" @click="store.closePanel()">✕</button>
    </div>
    <div class="body">
      <SessionsPanel v-if="store.panel === 'sessions'" />
      <SkillsPanel v-else-if="store.panel === 'skills'" />
      <McpPanel v-else-if="store.panel === 'mcp'" />
      <AgentsPanel v-else-if="store.panel === 'agents'" />
    </div>
  </div>
</template>

<style scoped>
.panel-page {
  position: fixed;
  inset: 0;
  z-index: 40;
  display: flex;
  flex-direction: column;
  background: var(--bg);
}

.head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 10px 20px;
  border-bottom: 1px solid var(--border);
  background: var(--bg-elev);
}

.title {
  color: var(--accent);
  font-weight: bold;
  text-transform: uppercase;
  font-size: 12px;
  letter-spacing: 0.08em;
}

.close {
  background: none;
  border: 1px solid var(--border);
  color: var(--fg-dim);
  padding: 2px 10px;
  cursor: pointer;
}

.close:hover {
  color: var(--fg);
  border-color: var(--accent);
}

.body {
  flex: 1;
  overflow: hidden;
  display: flex;
}
</style>
