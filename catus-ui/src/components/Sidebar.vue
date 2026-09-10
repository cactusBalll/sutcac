<script setup lang="ts">
import { computed } from 'vue';
import { useRuntimeStore } from '../stores/runtime';

const store = useRuntimeStore();

function firstPrompt(summary: string): string {
  return summary.length > 50 ? `${summary.slice(0, 50)}…` : summary;
}

function formatDate(ts: number): string {
  const d = new Date(ts);
  return `${d.getMonth() + 1}/${d.getDate()} ${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

async function resume(name: string) {
  await store.pick('resume_history', name);
}

const disabledSkills = computed(() => store.skillCatalog.filter((s) => s.disabled).length);
const disabledAgents = computed(() => store.agentCatalog.filter((a) => a.disabled).length);
const disabledMcp = computed(() => store.mcpServers.filter((s) => !s.enabled).length);
</script>

<template>
  <aside class="sidebar" :class="{ collapsed: !store.sidebarOpen }">
    <button class="toggle" :title="store.sidebarOpen ? 'collapse' : 'expand'" @click="store.sidebarOpen = !store.sidebarOpen">
      {{ store.sidebarOpen ? '‹' : '›' }}
    </button>

    <button class="new primary" title="save this session and start a new context" @click="store.newContext()">
      <span class="icon">+</span>
      <span v-if="store.sidebarOpen" class="label">new context</span>
    </button>

    <div class="section">
      <div class="section-head">
        <button class="entry" title="history sessions" @click="store.openPanel('sessions')">
          <span class="icon">≡</span>
          <span v-if="store.sidebarOpen" class="label">history</span>
          <span v-if="store.sidebarOpen" class="badge">{{ store.recentSessions.length }}</span>
        </button>
      </div>
      <template v-if="store.sidebarOpen">
        <button
          v-for="s in store.recentSessions"
          :key="s.id"
          class="session"
          :title="`${s.name}\n${s.summary}`"
          @click="resume(s.name)"
        >
          <span class="s-name">{{ s.name }}</span>
          <span v-if="s.summary" class="s-summary">{{ firstPrompt(s.summary) }}</span>
          <span class="s-date">{{ formatDate(s.updated_at) }}</span>
        </button>
        <button v-if="store.recentSessions.length" class="expand" @click="store.openPanel('sessions')">
          all sessions ›
        </button>
      </template>
    </div>

    <div class="spacer" />

    <button class="entry" title="Agent Skills" @click="store.openPanel('skills')">
      <span class="icon">✦</span>
      <span v-if="store.sidebarOpen" class="label">skills</span>
      <span v-if="store.sidebarOpen && disabledSkills" class="badge warn">{{ disabledSkills }}</span>
    </button>
    <button class="entry" title="MCP servers" @click="store.openPanel('mcp')">
      <span class="icon">⇄</span>
      <span v-if="store.sidebarOpen" class="label">mcp</span>
      <span v-if="store.sidebarOpen && disabledMcp" class="badge warn">{{ disabledMcp }}</span>
    </button>
    <button class="entry" title="Agents" @click="store.openPanel('agents')">
      <span class="icon">◇</span>
      <span v-if="store.sidebarOpen" class="label">agents</span>
      <span v-if="store.sidebarOpen && disabledAgents" class="badge warn">{{ disabledAgents }}</span>
    </button>
  </aside>
</template>

<style scoped>
.sidebar {
  width: 208px;
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 10px 8px;
  background: var(--bg-elev);
  border-right: 1px solid var(--border);
  overflow-y: auto;
}

.sidebar.collapsed {
  width: 44px;
  align-items: center;
}

.sidebar.collapsed .entry,
.sidebar.collapsed .new,
.sidebar.collapsed .session {
  width: 100%;
  justify-content: center;
}

.toggle {
  align-self: flex-end;
  background: none;
  border: none;
  color: var(--fg-dim);
  padding: 0 4px;
  font-size: 14px;
  cursor: pointer;
}

.new {
  display: flex;
  align-items: center;
  gap: 8px;
  margin: 4px 0 8px;
  text-align: left;
}

.section {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.entry {
  display: flex;
  align-items: center;
  gap: 8px;
  text-align: left;
  background: none;
  border: none;
  border-radius: 4px;
  padding: 5px 8px;
  color: var(--fg-dim);
  cursor: pointer;
  width: 100%;
}

.entry:hover {
  color: var(--fg);
  background: var(--accent-dim);
}

.entry .icon {
  width: 16px;
  text-align: center;
  flex-shrink: 0;
}

.label {
  flex: 1;
}

.badge {
  font-size: 10px;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 0 6px;
}

.badge.warn {
  color: var(--warn);
  border-color: var(--warn);
}

.session {
  display: flex;
  flex-direction: column;
  align-items: stretch;
  gap: 1px;
  text-align: left;
  background: none;
  border: none;
  border-radius: 4px;
  padding: 4px 8px 4px 32px;
  color: var(--fg-dim);
  cursor: pointer;
}

.session:hover {
  background: var(--accent-dim);
  color: var(--fg);
}

.s-name {
  font-size: 12px;
  color: var(--fg);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.s-summary {
  font-size: 11px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.s-date {
  font-size: 10px;
  color: var(--fg-dim);
}

.expand {
  background: none;
  border: none;
  color: var(--accent);
  text-align: left;
  padding: 4px 8px 4px 32px;
  font-size: 11px;
  cursor: pointer;
}

.expand:hover {
  text-decoration: underline;
}

.spacer {
  flex: 1;
}
</style>
