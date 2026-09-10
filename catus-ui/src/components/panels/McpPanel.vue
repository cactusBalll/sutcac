<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue';
import type {
  ConfigScope,
  ConfigScopeSnapshot,
  McpServerEntry,
} from '../../types';
import { useRuntimeStore } from '../../stores/runtime';

const store = useRuntimeStore();

/** Servers with a connect request in flight (cleared by the next snapshot). */
const connecting = ref<Set<string>>(new Set());

/** Which config file is being edited. */
const tab = ref<ConfigScope>('workspace');

const scopes = computed<ConfigScopeSnapshot[]>(() => store.configScopes);
const activeScope = computed<ConfigScopeSnapshot | undefined>(() =>
  scopes.value.find((s) => s.scope === tab.value),
);
const workspaceScope = computed<ConfigScopeSnapshot | undefined>(() =>
  scopes.value.find((s) => s.scope === 'workspace'),
);

watch(
  () => store.configScopes,
  (list) => {
    for (const scope of list) {
      for (const server of scope.mcp_servers) {
        const key = draftKey(scope.scope, server.name);
        if (!(key in drafts)) drafts[key] = entryToDraft(server);
      }
    }
  },
  { immediate: true, deep: true },
);

watch(
  () => store.mcpServers,
  () => {
    connecting.value = new Set();
  },
  { deep: true },
);

async function connect(name: string) {
  connecting.value = new Set(connecting.value).add(name);
  await store.connectMcp(name);
  connecting.value = new Set([...connecting.value].filter((n) => n !== name));
}

/* ------------------------------------------------------------------ */
/* `[[mcp.servers]]` editing (the active scope tab).                   */
/* ------------------------------------------------------------------ */

/** Editing form for one entry: list-ish fields become single-line text. */
interface McpDraft {
  name: string;
  transport: 'stdio' | 'streamable-http';
  command: string;
  args: string;
  env: string;
  url: string;
  headers: string;
}

/** Local edit drafts, keyed by `scope:name`; (re)initialized per snapshot. */
const drafts = reactive<Record<string, McpDraft>>({});
/** Rows that just saved successfully (transient "saved ✓" feedback). */
const savedKeys = reactive<Set<string>>(new Set());
const error = ref('');
/** New-server form. */
const newServer = ref<McpDraft>(emptyDraft());

function emptyDraft(): McpDraft {
  return { name: '', transport: 'stdio', command: '', args: '', env: '', url: '', headers: '' };
}

function draftKey(scope: ConfigScope, name: string) {
  return `${scope}:${name}`;
}

function pairsToText(map: Record<string, string>): string {
  return Object.entries(map)
    .map(([k, v]) => `${k}=${v}`)
    .join(', ');
}

/** Comma-separated `key=value` list → map (malformed items are dropped). */
function textToPairs(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const part of text.split(',')) {
    const item = part.trim();
    if (!item) continue;
    const eq = item.indexOf('=');
    if (eq <= 0) continue;
    out[item.slice(0, eq).trim()] = item.slice(eq + 1).trim();
  }
  return out;
}

function entryToDraft(entry: McpServerEntry): McpDraft {
  return {
    name: entry.name,
    transport: entry.transport,
    command: entry.command,
    args: entry.args.join(' '),
    env: pairsToText(entry.env),
    url: entry.url ?? '',
    headers: pairsToText(entry.headers),
  };
}

function draftToEntry(draft: McpDraft, name: string): McpServerEntry {
  return {
    name,
    transport: draft.transport,
    command: draft.command.trim(),
    args: draft.args.split(/\s+/).filter(Boolean),
    env: textToPairs(draft.env),
    url: draft.url.trim(),
    headers: textToPairs(draft.headers),
  };
}

function savedOf(scope: ConfigScopeSnapshot, name: string): McpServerEntry | undefined {
  return scope.mcp_servers.find((s) => s.name === name);
}

function draftOf(scope: ConfigScopeSnapshot, name: string): McpDraft {
  const key = draftKey(scope.scope, name);
  const existing = drafts[key];
  if (existing) return existing;
  const saved = savedOf(scope, name);
  const draft = saved ? entryToDraft(saved) : emptyDraft();
  drafts[key] = draft;
  return draft;
}

/** The scope whose entry for `name` is effective (workspace wins). */
function winningScope(name: string): ConfigScope | null {
  if (workspaceScope.value?.mcp_servers.some((s) => s.name === name)) return 'workspace';
  if (scopes.value.find((s) => s.scope === 'global')?.mcp_servers.some((s) => s.name === name)) {
    return 'global';
  }
  return null;
}

function isOverridden(name: string, scope: ConfigScopeSnapshot) {
  return scope.scope === 'global' && winningScope(name) === 'workspace';
}

function isCurrent(name: string) {
  return store.mcpServers.some((s) => s.name === name && s.connected);
}

function isDirty(scope: ConfigScopeSnapshot, name: string) {
  const draft = drafts[draftKey(scope.scope, name)];
  if (!draft) return false;
  const saved = savedOf(scope, name);
  if (!saved) return true;
  const entry = draftToEntry(draft, name);
  return (
    entry.transport !== saved.transport ||
    entry.command !== saved.command ||
    entry.args.join(' ') !== saved.args.join(' ') ||
    pairsToText(entry.env) !== pairsToText(saved.env) ||
    (entry.url ?? '') !== (saved.url ?? '') ||
    pairsToText(entry.headers) !== pairsToText(saved.headers)
  );
}

async function save(scope: ConfigScopeSnapshot, name: string) {
  error.value = '';
  const draft = drafts[draftKey(scope.scope, name)];
  if (!draft) return;
  if (!name.trim()) {
    error.value = 'the server name is required';
    return;
  }
  const ok = await store.saveMcpServer(scope.scope, draftToEntry(draft, name.trim()));
  if (ok) {
    savedKeys.add(draftKey(scope.scope, name));
    setTimeout(() => savedKeys.delete(draftKey(scope.scope, name)), 1500);
  } else {
    error.value = store.statusMessage || 'saving the server failed';
  }
}

async function remove(scope: ConfigScopeSnapshot, name: string) {
  error.value = '';
  const ok = await store.deleteMcpServer(scope.scope, name);
  if (!ok) error.value = store.statusMessage || 'removing the server failed';
  delete drafts[draftKey(scope.scope, name)];
}

async function add() {
  error.value = '';
  if (!activeScope.value) return;
  const draft = newServer.value;
  if (!draft.name.trim()) {
    error.value = 'the server name is required';
    return;
  }
  if (savedOf(activeScope.value, draft.name.trim())) {
    error.value = `server '${draft.name.trim()}' already exists in this scope`;
    return;
  }
  if (draft.transport === 'stdio' && !draft.command.trim()) {
    error.value = 'the command is required for the stdio transport';
    return;
  }
  if (draft.transport === 'streamable-http' && !draft.url.trim()) {
    error.value = 'the url is required for the streamable-http transport';
    return;
  }
  const ok = await store.saveMcpServer(tab.value, draftToEntry(draft, draft.name.trim()));
  if (ok) {
    newServer.value = emptyDraft();
  } else {
    error.value = store.statusMessage || 'adding the server failed';
  }
}

function transportLabel(entry: McpServerEntry): string {
  return entry.transport === 'stdio' ? 'stdio' : 'http';
}
</script>

<template>
  <div class="page">
    <div v-if="!store.mcpServers.length && !scopes.some((s) => s.mcp_servers.length)" class="empty">
      no MCP servers configured
    </div>

    <template v-for="server in store.mcpServers" :key="server.name">
      <div class="server">
        <div class="row">
          <span class="name">{{ server.name }}</span>
          <span class="state" :class="{ ok: server.connected && server.enabled, dim: !server.connected }">
            {{ server.connected ? 'connected' : 'not connected' }}
            <template v-if="!server.enabled"> · disabled</template>
          </span>
          <div class="actions">
            <button
              v-if="server.enabled"
              class="connect"
              :disabled="connecting.has(server.name)"
              :title="server.connected ? 'reconnect: drop the (possibly dead) connection and reconnect' : 'connect to this server now'"
              @click="connect(server.name)"
            >
              {{ connecting.has(server.name) ? 'connecting…' : server.connected ? 'reconnect' : 'connect' }}
            </button>
            <button
              class="switch"
              :class="{ off: !server.enabled }"
              :title="server.enabled ? 'temporarily disable (the tool rejects actions; the tool list is unchanged)' : 're-enable'"
              @click="store.toggleMcp(server.name)"
            >
              {{ server.enabled ? 'enabled' : 'disabled' }}
            </button>
          </div>
        </div>
        <div v-if="server.connected && server.tools.length" class="tools">
          <span v-for="tool in server.tools" :key="tool" class="tool">{{ tool }}</span>
          <span v-if="!server.tools.length" class="dim">(no tools)</span>
        </div>
        <div v-else-if="server.connected" class="tools dim">(no tools)</div>
      </div>
    </template>

    <div class="section-head">
      <span class="effective-label">[[mcp.servers]] configuration</span>
    </div>

    <div class="tabs">
      <button class="tab" :class="{ active: tab === 'workspace' }" @click="tab = 'workspace'">
        workspace
        <span class="tag" :class="{ ok: scopes[0]?.exists, warn: !scopes[0]?.exists }">
          {{ scopes[0]?.exists ? 'exists' : 'not created' }}
        </span>
      </button>
      <button class="tab" :class="{ active: tab === 'global' }" @click="tab = 'global'">
        global
        <span class="tag" :class="{ ok: scopes[1]?.exists, warn: !scopes[1]?.exists }">
          {{ scopes[1]?.exists ? 'exists' : 'not created' }}
        </span>
      </button>
    </div>

    <template v-if="activeScope">
      <div class="path">{{ activeScope.path }}</div>

      <div v-if="!activeScope.mcp_servers.length" class="empty">
        no [[mcp.servers]] entries in this scope
      </div>

      <div v-for="server in activeScope.mcp_servers" :key="server.name" class="entry">
        <div class="entry-head">
          <span class="id" :title="server.name">
            {{ server.name }}
            <span class="tag">{{ transportLabel(server) }}</span>
            <span v-if="isCurrent(server.name)" class="tag ok">connected</span>
            <span v-if="isOverridden(server.name, activeScope)" class="tag">
              overridden by workspace
            </span>
          </span>
          <select
            v-model="draftOf(activeScope, server.name).transport"
            class="value transport"
            spellcheck="false"
          >
            <option value="stdio">stdio</option>
            <option value="streamable-http">streamable-http</option>
          </select>
          <button
            class="btn"
            :class="{ dirty: isDirty(activeScope, server.name), saved: savedKeys.has(draftKey(activeScope.scope, server.name)) }"
            @click="save(activeScope, server.name)"
          >
            {{ savedKeys.has(draftKey(activeScope.scope, server.name)) ? 'saved ✓' : 'save' }}
          </button>
          <button class="btn danger" title="remove this [[mcp.servers]] entry" @click="remove(activeScope, server.name)">
            delete
          </button>
        </div>

        <div v-if="draftOf(activeScope, server.name).transport === 'stdio'" class="fields">
          <label class="field">
            <span>command</span>
            <input
              v-model="draftOf(activeScope, server.name).command"
              class="value"
              placeholder="executable, resolved via PATH"
              spellcheck="false"
            />
          </label>
          <label class="field">
            <span>args</span>
            <input
              v-model="draftOf(activeScope, server.name).args"
              class="value"
              placeholder="--flag value (space-separated)"
              spellcheck="false"
            />
          </label>
          <label class="field">
            <span>env</span>
            <input
              v-model="draftOf(activeScope, server.name).env"
              class="value"
              placeholder="KEY=value, KEY2=value2"
              spellcheck="false"
            />
          </label>
        </div>
        <div v-else class="fields">
          <label class="field">
            <span>url</span>
            <input
              v-model="draftOf(activeScope, server.name).url"
              class="value"
              placeholder="http://127.0.0.1:8000/mcp"
              spellcheck="false"
            />
          </label>
          <label class="field">
            <span>headers</span>
            <input
              v-model="draftOf(activeScope, server.name).headers"
              class="value"
              placeholder="Authorization=Bearer …, X-Custom=v"
              spellcheck="false"
            />
          </label>
        </div>
      </div>

      <div class="entry add">
        <div class="entry-head">
          <input v-model="newServer.name" class="value name" placeholder="new server name" spellcheck="false" />
          <select v-model="newServer.transport" class="value transport" spellcheck="false">
            <option value="stdio">stdio</option>
            <option value="streamable-http">streamable-http</option>
          </select>
          <button class="btn primary" @click="add">+ add server</button>
        </div>
        <div v-if="newServer.transport === 'stdio'" class="fields">
          <label class="field">
            <span>command</span>
            <input v-model="newServer.command" class="value" placeholder="executable" spellcheck="false" />
          </label>
          <label class="field">
            <span>args</span>
            <input v-model="newServer.args" class="value" placeholder="space-separated" spellcheck="false" />
          </label>
          <label class="field">
            <span>env</span>
            <input v-model="newServer.env" class="value" placeholder="KEY=value, …" spellcheck="false" />
          </label>
        </div>
        <div v-else class="fields">
          <label class="field">
            <span>url</span>
            <input v-model="newServer.url" class="value" placeholder="http://…" spellcheck="false" />
          </label>
          <label class="field">
            <span>headers</span>
            <input v-model="newServer.headers" class="value" placeholder="KEY=value, …" spellcheck="false" />
          </label>
        </div>
      </div>

      <div v-if="error" class="error">{{ error }}</div>
      <div class="hint">
        {{ activeScope.scope === 'workspace' ? 'saved entries override the global config (matched by name)' : 'shared by all workspaces; workspace entries with the same name win' }}
        · the name is the entry key: to rename, delete the entry and add it again
        · editing an entry drops its live connection; use the connect button above to reconnect
      </div>
    </template>
  </div>
</template>

<style scoped>
.page {
  flex: 1;
  overflow-y: auto;
  padding: 12px 20px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.server {
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 10px 14px;
}

.row {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}

.name {
  color: var(--fg);
  font-weight: bold;
  flex: 1;
}

.state {
  font-size: 11px;
  color: var(--ok);
}

.state.dim {
  color: var(--fg-dim);
}

.actions {
  display: flex;
  gap: 8px;
}

.connect {
  font-size: 11px;
  padding: 1px 10px;
  background: none;
  color: var(--accent);
  border-color: var(--accent);
}

.connect:disabled {
  color: var(--fg-dim);
  border-color: var(--border);
  cursor: default;
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

.tools {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 8px;
}

.tool {
  font-size: 11px;
  color: var(--fg-dim);
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 0 8px;
}

.section-head {
  margin-top: 6px;
}

.effective-label {
  color: var(--fg-dim);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.06em;
}

.tabs {
  display: flex;
  gap: 6px;
}

.tab {
  font: inherit;
  font-size: 12px;
  color: var(--fg-dim);
  background: none;
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 3px 10px;
  cursor: pointer;
  display: flex;
  align-items: center;
  gap: 6px;
}

.tab.active {
  color: var(--fg);
  border-color: var(--accent);
  background: var(--accent-dim);
}

.tag {
  color: var(--fg-dim);
  font-size: 10px;
  border: 1px solid var(--border);
  border-radius: 3px;
  padding: 0 5px;
}

.tag.ok {
  color: var(--ok);
}

.path {
  color: var(--fg-dim);
  font-size: 11px;
  word-break: break-all;
}

.empty {
  color: var(--fg-dim);
  font-size: 12px;
}

.entry {
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.entry.add {
  border-style: dashed;
}

.entry-head {
  display: flex;
  align-items: center;
  gap: 8px;
}

.id {
  width: 160px;
  flex-shrink: 0;
  color: var(--fg);
  font-weight: bold;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.fields {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

.field {
  display: flex;
  align-items: center;
  gap: 6px;
  flex: 1 1 260px;
  min-width: 220px;
}

.field > span {
  color: var(--fg-dim);
  font-size: 11px;
  width: 52px;
  flex-shrink: 0;
  text-align: right;
}

.field .value {
  flex: 1;
  min-width: 0;
}

.value {
  font: inherit;
  font-size: 12px;
  color: var(--fg);
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 4px 8px;
  outline: none;
}

.value:focus {
  border-color: var(--accent);
}

.transport {
  flex: 0 0 140px;
}

.entry-head .value.name {
  flex: 0 0 200px;
}

.btn {
  font: inherit;
  font-size: 11px;
  color: var(--fg);
  background: var(--accent-dim);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 3px 10px;
  cursor: pointer;
  flex-shrink: 0;
}

.btn:hover {
  border-color: var(--accent);
}

.btn.dirty {
  border-color: var(--accent);
  color: var(--accent);
}

.btn.saved {
  border-color: var(--ok);
  color: var(--ok);
}

.btn.danger:hover {
  border-color: var(--err);
  color: var(--err);
}

.btn.primary {
  color: var(--accent);
  border-color: var(--accent);
}

.error {
  color: var(--err);
  font-size: 12px;
}

.hint {
  color: var(--fg-dim);
  font-size: 11px;
}

.dim {
  color: var(--fg-dim);
  font-size: 11px;
}
</style>
