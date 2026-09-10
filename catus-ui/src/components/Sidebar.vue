<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue';
import { useRuntimeStore } from '../stores/runtime';

const store = useRuntimeStore();

/** Narrow viewport (phone / narrow window): the open sidebar overlays the
 *  chat as a drawer, and the app starts with the icon rail. */
const narrow = ref(window.matchMedia('(max-width: 768px)').matches);

function onNarrowChange(event: MediaQueryListEvent) {
  narrow.value = event.matches;
  // Auto-collapse when entering the narrow layout; leaving it narrow does
  // not force the drawer back open.
  if (event.matches) store.sidebarOpen = false;
}

function closeDrawer() {
  store.sidebarOpen = false;
}

onMounted(() =>
  window.matchMedia('(max-width: 768px)').addEventListener('change', onNarrowChange),
);
onUnmounted(() =>
  window.matchMedia('(max-width: 768px)').removeEventListener('change', onNarrowChange),
);

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
  <div
    v-if="narrow && store.sidebarOpen"
    class="drawer-backdrop"
    @click="closeDrawer"
  />
  <aside class="sidebar" :class="{ collapsed: !store.sidebarOpen, drawer: narrow && store.sidebarOpen }">
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
        <div v-if="!store.recentSessions.length" class="no-sessions">no sessions yet</div>
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
    <button class="entry" title="Models" @click="store.openPanel('models')">
      <span class="icon">◆</span>
      <span v-if="store.sidebarOpen" class="label">models</span>
      <span v-if="store.sidebarOpen" class="badge">{{ store.models.length }}</span>
    </button>
    <button class="entry" title="config editor" @click="store.openConfigPage()">
      <span class="icon">⚙</span>
      <span v-if="store.sidebarOpen" class="label">config</span>
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

.drawer-backdrop {
  position: fixed;
  inset: 0;
  z-index: 29;
  background: rgba(6, 8, 12, 0.6);
}

/* On narrow viewports the expanded sidebar floats over the chat as a
   drawer; the icon rail stays in the flex flow. */
.sidebar.drawer {
  position: fixed;
  left: 0;
  top: 0;
  bottom: 0;
  z-index: 30;
  box-shadow: 4px 0 16px rgba(0, 0, 0, 0.45);
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

.no-sessions {
  color: var(--fg-dim);
  font-size: 11px;
  padding: 4px 8px 4px 32px;
}

.spacer {
  flex: 1;
}
</style>
