<script setup lang="ts">
import { computed } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';
import type { SessionSummary } from '../../types';

const store = useRuntimeStore();

interface Group {
  cwd: string;
  current: boolean;
  sessions: SessionSummary[];
}

const groups = computed<Group[]>(() => {
  const map = new Map<string, SessionSummary[]>();
  for (const s of store.allSessions) {
    const key = s.cwd || '(unknown workspace)';
    if (!map.has(key)) map.set(key, []);
    map.get(key)!.push(s);
  }
  const list: Group[] = [...map.entries()].map(([cwd, sessions]) => ({
    cwd,
    current: cwd === store.sessionCwd,
    sessions,
  }));
  // The current workspace's group comes first.
  list.sort((a, b) => Number(b.current) - Number(a.current) || a.cwd.localeCompare(b.cwd));
  return list;
});

function firstPrompt(summary: string): string {
  return summary.length > 50 ? `${summary.slice(0, 50)}…` : summary;
}

function formatDate(ts: number): string {
  const d = new Date(ts);
  return d.toLocaleString();
}

async function resume(name: string) {
  store.closePanel();
  await store.pick('resume_history', name);
}

async function refresh() {
  await store.refreshSessions();
}
</script>

<template>
  <div class="page">
    <div class="toolbar">
      <span class="hint">click a session to resume it · {{ store.allSessions.length }} session(s)</span>
      <button @click="refresh">refresh</button>
    </div>
    <div class="list">
      <section v-for="group in groups" :key="group.cwd" class="group">
        <div class="group-head">
          {{ group.cwd }}
          <span v-if="group.current" class="tag">current</span>
        </div>
        <button
          v-for="s in group.sessions"
          :key="s.id"
          class="session"
          :class="{ current: group.current }"
          :title="`${s.name} · ${s.model_id}`"
          @click="resume(s.name)"
        >
          <span class="row">
            <span class="s-name">{{ s.name }}</span>
            <span class="s-date">{{ formatDate(s.updated_at) }}</span>
          </span>
          <span class="s-summary">{{ firstPrompt(s.summary) || '(no user prompt yet)' }}</span>
        </button>
      </section>
      <div v-if="!groups.length" class="empty">no saved sessions</div>
    </div>
  </div>
</template>

<style scoped>
.page {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.toolbar {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 10px 20px;
  color: var(--fg-dim);
  font-size: 12px;
  flex-wrap: wrap;
}

.hint {
  flex: 1;
}

.list {
  flex: 1;
  overflow-y: auto;
  padding: 0 20px 20px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}

.group-head {
  color: var(--accent);
  font-size: 12px;
  font-weight: bold;
  padding: 6px 0;
  border-bottom: 1px solid var(--border);
  display: flex;
  align-items: center;
  gap: 8px;
}

.tag {
  font-size: 10px;
  text-transform: uppercase;
  color: var(--ok);
  border: 1px solid var(--ok);
  border-radius: 8px;
  padding: 0 6px;
}

.session {
  display: flex;
  flex-direction: column;
  gap: 2px;
  text-align: left;
  background: none;
  border: none;
  border-bottom: 1px solid var(--border);
  border-radius: 0;
  padding: 8px 4px;
  color: var(--fg-dim);
  cursor: pointer;
}

.session:hover {
  background: var(--accent-dim);
}

.session.current .s-name {
  color: var(--accent);
}

.row {
  display: flex;
  justify-content: space-between;
  gap: 12px;
}

.s-name {
  color: var(--fg);
  font-size: 13px;
}

.s-date {
  font-size: 11px;
  color: var(--fg-dim);
}

.s-summary {
  font-size: 12px;
  color: var(--fg-dim);
}

.empty {
  color: var(--fg-dim);
  padding: 24px 0;
}
</style>
