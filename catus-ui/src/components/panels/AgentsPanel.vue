<script setup lang="ts">
import { ref } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';
import type { AgentDetail } from '../../types';

const store = useRuntimeStore();

/** Which agent's editor is open (`null` = closed; `''` = new agent). */
const editing = ref<string | null>(null);
const editorContent = ref('');
const editorError = ref('');
const editorSaving = ref(false);

const NEW_TEMPLATE = `---
name: new-agent
description: Describe what this agent is for.
tools:
  - inherit
skills:
  - inherit
---

You are a focused subagent. Describe the agent's role, workflow, and
reporting expectations here. The body becomes the agent's system prompt.
`;

function roleBadge(role: string | null): string {
  if (role === 'main') return 'main';
  if (role === 'memory') return 'memory';
  return '';
}

async function openEditor(name: string) {
  editorError.value = '';
  if (name === '') {
    editing.value = '';
    editorContent.value = NEW_TEMPLATE;
    return;
  }
  try {
    const detail: AgentDetail = await store.agentDetail(name);
    editing.value = name;
    editorContent.value = detail.content;
  } catch (e) {
    editorError.value = String(e);
  }
}

function closeEditor() {
  editing.value = null;
  editorContent.value = '';
  editorError.value = '';
}

async function save() {
  editorSaving.value = true;
  editorError.value = '';
  const content = editorContent.value;
  const target = editing.value;
  try {
    if (target === '') {
      // Derive the agent name from the `name:` frontmatter field.
      const match = content.match(/^name:\s*(\S+)\s*$/m);
      const name = match?.[1];
      if (!name) {
        editorError.value = "the frontmatter must contain a `name:` field";
        return;
      }
      await store.createAgent(name, content);
    } else if (target !== null) {
      await store.saveAgent(target, content);
    }
    closeEditor();
  } catch (e) {
    editorError.value = String(e);
  } finally {
    editorSaving.value = false;
  }
}
</script>

<template>
  <div class="page">
    <div class="list">
      <div class="toolbar">
        <button class="primary" @click="openEditor('')">+ new agent</button>
      </div>
      <div v-if="!store.agentCatalog.length" class="empty">no agents discovered</div>
      <div v-for="agent in store.agentCatalog" :key="agent.name" class="item">
        <div class="row">
          <span class="name">
            {{ agent.name }}
            <span v-if="agent.role" class="role">{{ roleBadge(agent.role) }}</span>
            <span v-if="agent.disabled" class="role off">disabled</span>
          </span>
          <div class="actions">
            <button
              v-if="agent.editable"
              class="edit"
              title="edit the agent definition"
              @click="openEditor(agent.name)"
            >
              edit
            </button>
            <button
              v-else
              class="edit"
              title="special-role agents are preview-only"
              @click="openEditor(agent.name)"
            >
              view
            </button>
            <button
              v-if="agent.editable"
              class="switch"
              :class="{ off: agent.disabled }"
              :title="agent.disabled ? 're-enable dispatch' : 'temporarily disable dispatch'"
              @click="store.toggleAgent(agent.name)"
            >
              {{ agent.disabled ? 'disabled' : 'enabled' }}
            </button>
          </div>
        </div>
        <div class="desc">{{ agent.description }}</div>
      </div>
    </div>

    <div v-if="editing !== null" class="editor">
      <div class="editor-head">
        <span>{{ editing === '' ? 'new agent' : `edit ${editing}` }}</span>
        <div class="editor-actions">
          <button :disabled="editorSaving" @click="save">save</button>
          <button @click="closeEditor">cancel</button>
        </div>
      </div>
      <div v-if="editorError" class="editor-error">{{ editorError }}</div>
      <textarea v-model="editorContent" class="content" spellcheck="false" />
      <div class="editor-hint">
        raw agent definition: YAML frontmatter + Markdown body (the body is the
        system prompt). Special-role agents (main, memory) are read-only.
      </div>
    </div>
  </div>
</template>

<style scoped>
.page {
  flex: 1;
  display: flex;
  min-width: 0;
}

.list {
  width: 40%;
  min-width: 300px;
  overflow-y: auto;
  border-right: 1px solid var(--border);
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}

.toolbar {
  display: flex;
  justify-content: flex-end;
  margin-bottom: 4px;
}

.toolbar button {
  font-size: 12px;
  padding: 2px 10px;
}

.item {
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 8px 12px;
}

.row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
}

.name {
  color: var(--fg);
  font-weight: bold;
}

.role {
  font-size: 10px;
  text-transform: uppercase;
  color: var(--warn);
  border: 1px solid var(--warn);
  border-radius: 8px;
  padding: 0 6px;
  margin-left: 6px;
}

.role.off {
  color: var(--fg-dim);
  border-color: var(--border);
}

.desc {
  color: var(--fg-dim);
  font-size: 12px;
  margin-top: 2px;
}

.actions {
  display: flex;
  gap: 8px;
}

.edit {
  font-size: 11px;
  padding: 1px 10px;
  background: none;
}

.switch {
  font-size: 11px;
  padding: 1px 10px;
  border-radius: 10px;
  color: var(--ok);
  border-color: var(--ok);
  background: none;
}

.switch.off {
  color: var(--fg-dim);
  border-color: var(--border);
}

.editor {
  flex: 1;
  display: flex;
  flex-direction: column;
  padding: 12px 20px;
  min-width: 0;
}

.editor-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  color: var(--accent);
  font-weight: bold;
  margin-bottom: 8px;
}

.editor-actions {
  display: flex;
  gap: 8px;
}

.editor-actions button {
  font-size: 12px;
  padding: 2px 12px;
}

.editor-error {
  color: var(--err);
  font-size: 12px;
  margin-bottom: 8px;
}

.content {
  flex: 1;
  font: inherit;
  font-size: 12px;
  color: var(--fg);
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 10px;
  resize: none;
  white-space: pre;
}

.content:focus {
  outline: none;
  border-color: var(--accent);
}

.editor-hint {
  color: var(--fg-dim);
  font-size: 11px;
  margin-top: 8px;
}

.empty {
  color: var(--fg-dim);
  padding: 12px 0;
}

@media (max-width: 640px) {
  .page {
    flex-direction: column;
    overflow-y: auto;
  }

  .list {
    width: 100%;
    min-width: 0;
    border-right: none;
    border-bottom: 1px solid var(--border);
    overflow-y: visible;
  }
}
</style>
