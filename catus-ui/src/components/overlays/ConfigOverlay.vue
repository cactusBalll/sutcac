<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue';
import type {
  ConfigFieldSpec,
  ConfigScope,
  ConfigScopeSnapshot,
} from 'catus-ui';
import { useRuntimeStore } from '../../stores/runtime';

const store = useRuntimeStore();

/** Active tab: the effective values or one of the two editable scopes. */
const tab = ref<'effective' | ConfigScope>('effective');

/** Local edit drafts, keyed by `scope:key`; (re)initialized per snapshot. */
const drafts = reactive<Record<string, string>>({});

const scopes = computed<ConfigScopeSnapshot[]>(() => store.configScopes);
const specs = computed<ConfigFieldSpec[]>(() => store.configFieldSpecs);
const activeScope = computed<ConfigScopeSnapshot | undefined>(() =>
  scopes.value.find((s) => s.scope === tab.value),
);

function draftKey(scope: ConfigScope, key: string) {
  return `${scope}:${key}`;
}

watch(
  () => store.configScopes,
  (list) => {
    for (const scope of list) {
      for (const [key, value] of scope.fields) {
        const id = draftKey(scope.scope, key);
        if (!(id in drafts)) drafts[id] = value;
      }
    }
  },
  { immediate: true, deep: true },
);

function specOf(key: string): ConfigFieldSpec | undefined {
  return specs.value.find((spec) => spec.key === key);
}

/** The scope whose value for `key` is effective (workspace wins). */
function winningScope(key: string): ConfigScope | 'default' {
  const workspace = scopes.value.find((s) => s.scope === 'workspace');
  if (workspace?.fields.some(([k, v]) => k === key && v !== '')) return 'workspace';
  const global = scopes.value.find((s) => s.scope === 'global');
  if (global?.fields.some(([k, v]) => k === key && v !== '')) return 'global';
  return 'default';
}

function isOverridden(key: string, scope: ConfigScopeSnapshot) {
  return scope.scope === 'global' && winningScope(key) === 'workspace';
}

function isSet(scope: ConfigScopeSnapshot, key: string) {
  return scope.fields.some(([k, v]) => k === key && v !== '');
}

/** Draft value as a boolean (checkbox rows). */
function draftBool(scope: ConfigScope, key: string): boolean {
  const raw = drafts[draftKey(scope, key)] ?? '';
  if (raw === '') return false;
  return raw.toLowerCase() === 'true';
}

function setDraftBool(scope: ConfigScope, key: string, value: boolean) {
  drafts[draftKey(scope, key)] = value ? 'true' : 'false';
}

function onCheckbox(scope: ConfigScope, key: string, event: Event) {
  setDraftBool(scope, key, (event.target as HTMLInputElement).checked);
}

async function save(scope: ConfigScope, key: string) {
  // Coerce to string: v-model returns a number for `type="number"` inputs,
  // but the backend API expects string values for every field kind.
  const value = String(drafts[draftKey(scope, key)] ?? '');
  const ok = await store.setConfigField(scope, key, value);
  if (ok) drafts[draftKey(scope, key)] = value;
}

async function remove(scope: ConfigScope, key: string) {
  const ok = await store.removeConfigField(scope, key);
  if (ok) drafts[draftKey(scope, key)] = '';
}
</script>

<template>
  <div class="panel">
    <div class="head">config</div>

    <div class="tabs">
      <button
        class="tab"
        :class="{ active: tab === 'effective' }"
        @click="tab = 'effective'"
      >
        effective
      </button>
      <button
        class="tab"
        :class="{ active: tab === 'workspace' }"
        @click="tab = 'workspace'"
      >
        workspace
        <span class="tag" :class="{ ok: scopes[0]?.exists, warn: !scopes[0]?.exists }">
          {{ scopes[0]?.exists ? 'exists' : 'not created' }}
        </span>
      </button>
      <button
        class="tab"
        :class="{ active: tab === 'global' }"
        @click="tab = 'global'"
      >
        global
        <span class="tag" :class="{ ok: scopes[1]?.exists, warn: !scopes[1]?.exists }">
          {{ scopes[1]?.exists ? 'exists' : 'not created' }}
        </span>
      </button>
    </div>

    <!-- Effective values (read-only). -->
    <template v-if="tab === 'effective'">
      <div class="grid">
        <template v-for="[key, value] in store.configFields" :key="key">
          <span class="k" :title="specOf(key)?.description">{{ key }}</span>
          <span>
            {{ value === '' ? '(empty)' : value }}
            <span class="tag">{{ winningScope(key) }}</span>
          </span>
        </template>
      </div>
      <div class="hint">workspace values override global ones · edit on the other tabs</div>
    </template>

    <!-- One editable scope. -->
    <template v-else-if="activeScope">
      <div class="path">{{ activeScope.path }}</div>
      <div class="editor">
        <div v-for="[key, value] in activeScope.fields" :key="key" class="row">
          <span class="k" :title="specOf(key)?.description">
            {{ key }}
            <span class="desc">{{ specOf(key)?.description }}</span>
          </span>

          <label v-if="specOf(key)?.kind === 'bool'" class="bool">
            <input
              type="checkbox"
              :checked="draftBool(activeScope.scope, key)"
              @change="onCheckbox(activeScope.scope, key, $event)"
            />
            {{ drafts[draftKey(activeScope.scope, key)] || value || 'false' }}
          </label>

          <input
            v-else
            v-model="drafts[draftKey(activeScope.scope, key)]"
            class="value"
            :type="specOf(key)?.kind === 'int' ? 'number' : 'text'"
            :placeholder="value === '' ? (specOf(key)?.kind === 'list' ? 'a, b, c (not set)' : '(not set)') : value"
            spellcheck="false"
          />

          <button class="btn" @click="save(activeScope.scope, key)">save</button>
          <button
            v-if="isSet(activeScope, key)"
            class="btn danger"
            @click="remove(activeScope.scope, key)"
          >
            remove
          </button>
          <span v-if="isOverridden(key, activeScope)" class="tag warn">
            overridden by workspace
          </span>
        </div>
      </div>
      <div class="hint">
        {{
          activeScope.scope === 'workspace'
            ? 'saved values override the global config'
            : 'shared by all workspaces; per-key workspace overrides win'
        }}
        · list values are comma-separated
      </div>
    </template>
  </div>
</template>

<style scoped>
.panel {
  width: 680px;
  max-width: 92vw;
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
  margin-bottom: 10px;
}

.tabs {
  display: flex;
  gap: 6px;
  margin-bottom: 12px;
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

.tag.warn {
  color: #d8a24a;
}

.path {
  color: var(--fg-dim);
  font-size: 11px;
  margin-bottom: 8px;
  word-break: break-all;
}

.grid {
  display: grid;
  grid-template-columns: 220px 1fr;
  gap: 6px 14px;
}

.k {
  color: var(--fg-dim);
  display: inline-flex;
  flex-direction: column;
}

.desc {
  color: var(--fg-dim);
  opacity: 0.7;
  font-size: 10px;
}

.editor {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.row {
  display: flex;
  align-items: center;
  gap: 8px;
}

.row .k {
  width: 190px;
  flex-shrink: 0;
}

.bool {
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--fg);
  font-size: 12px;
  cursor: pointer;
}

.value {
  flex: 1;
  min-width: 0;
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

.btn {
  font: inherit;
  font-size: 11px;
  color: var(--fg);
  background: var(--accent-dim);
  border: 1px solid var(--border);
  border-radius: 4px;
  padding: 3px 10px;
  cursor: pointer;
}

.btn:hover {
  border-color: var(--accent);
}

.btn.danger:hover {
  border-color: var(--err);
  color: var(--err);
}

.hint {
  margin-top: 12px;
  color: var(--fg-dim);
  font-size: 11px;
}
</style>
