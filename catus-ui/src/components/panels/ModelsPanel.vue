<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue';
import type { ConfigScope, ConfigScopeSnapshot, ModelEntry } from '../../types';
import { useRuntimeStore } from '../../stores/runtime';

const store = useRuntimeStore();

/** Active tab: which config file is being edited. */
const tab = ref<ConfigScope>('workspace');

const scopes = computed<ConfigScopeSnapshot[]>(() => store.configScopes);
const activeScope = computed<ConfigScopeSnapshot | undefined>(() =>
  scopes.value.find((s) => s.scope === tab.value),
);
const workspaceScope = computed<ConfigScopeSnapshot | undefined>(() =>
  scopes.value.find((s) => s.scope === 'workspace'),
);

/** Local edit drafts, keyed by `scope:id`; (re)initialized per snapshot. */
const drafts = reactive<Record<string, ModelEntry>>({});
/** Rows that just saved successfully (transient "saved ✓" feedback). */
const savedKeys = reactive<Set<string>>(new Set());
const error = ref('');
/** New-model form. */
const newModel = reactive<ModelEntry>({ id: '', name: '', context_window: '', provider: '' });

/** `[agent.models]` tier keys editable on the active scope tab. */
const TIER_KEYS = ['agent.models.performance', 'agent.models.efficient'] as const;
type TierKey = (typeof TIER_KEYS)[number];

/** Tier drafts, keyed by `scope:key`; (re)initialized per snapshot. */
const tierDrafts = reactive<Record<string, string>>({});
/** Tier rows that just saved successfully (transient "saved ✓" feedback). */
const tierSavedKeys = reactive<Set<string>>(new Set());

function tierDraftKey(scope: ConfigScope, key: TierKey) {
  return `${scope}:${key}`;
}

/** The tier value as written in that scope's file ('' = not set there). */
function tierFieldOf(scope: ConfigScopeSnapshot, key: TierKey): string {
  return scope.fields.find(([k]) => k === key)?.[1] ?? '';
}

/** The effective tier value (workspace wins over global). */
function tierEffectiveOf(key: TierKey): string {
  return store.configFields.find(([k]) => k === key)?.[1] ?? '';
}

/** Which scope's tier value is effective (workspace wins). */
function tierWinningScope(key: TierKey): ConfigScope | 'default' {
  if (workspaceScope.value && tierFieldOf(workspaceScope.value, key)) return 'workspace';
  if (scopes.value.find((s) => s.scope === 'global')?.fields.some(([k, v]) => k === key && v !== '')) {
    return 'global';
  }
  return 'default';
}

function tierLabel(key: TierKey): string {
  return key.replace('agent.models.', '');
}

function tierEffectiveText(key: TierKey): string {
  const value = tierEffectiveOf(key);
  if (key === 'agent.models.efficient') return value || 'falls back to performance';
  return value || 'not set';
}

function tierDirty(key: TierKey) {
  if (!activeScope.value) return false;
  const id = tierDraftKey(tab.value, key);
  return tierDrafts[id] !== undefined && tierDrafts[id] !== tierFieldOf(activeScope.value, key);
}

/** Save the tier draft: an empty efficient value clears the key (falls
 *  back to performance); an empty performance value is rejected. */
async function saveTier(key: TierKey) {
  if (!activeScope.value) return;
  const id = tierDraftKey(tab.value, key);
  const value = (tierDrafts[id] ?? '').trim();
  if (key === 'agent.models.performance' && !value) {
    error.value = 'the performance tier is required';
    return;
  }
  const ok = value
    ? await store.setConfigField(tab.value, key, value)
    : await store.removeConfigField(tab.value, key);
  if (ok) {
    tierDrafts[id] = value;
    tierSavedKeys.add(id);
    setTimeout(() => tierSavedKeys.delete(id), 1500);
  } else {
    error.value = store.statusMessage || 'saving the tier failed';
  }
}

const providerNames = computed(() => [
  ...new Set(store.models.map((m) => m.provider_name).filter(Boolean)),
]);

function draftKey(scope: ConfigScope, id: string) {
  return `${scope}:${id}`;
}

/** The draft for one row, created on demand so a render can never read a
 *  missing entry (a new snapshot row arriving between renders). */
function draftOf(scope: ConfigScopeSnapshot, id: string): ModelEntry {
  const key = draftKey(scope.scope, id);
  const existing = drafts[key];
  if (existing) return existing;
  const saved = scope.models.find((m) => m.id === id);
  const draft: ModelEntry = saved
    ? { ...saved }
    : { id, name: '', context_window: '', provider: '' };
  drafts[key] = draft;
  return draft;
}

watch(
  () => store.configScopes,
  (list) => {
    for (const scope of list) {
      for (const model of scope.models) {
        const id = draftKey(scope.scope, model.id);
        if (!(id in drafts)) drafts[id] = { ...model };
      }
      for (const key of TIER_KEYS) {
        const id = tierDraftKey(scope.scope, key);
        if (!(id in tierDrafts)) tierDrafts[id] = tierFieldOf(scope, key);
      }
    }
  },
  { immediate: true, deep: true },
);

/** The scope whose entry for `id` is effective (workspace wins). */
function winningScope(id: string): ConfigScope | null {
  if (workspaceScope.value?.models.some((m) => m.id === id)) return 'workspace';
  if (scopes.value.find((s) => s.scope === 'global')?.models.some((m) => m.id === id)) {
    return 'global';
  }
  return null;
}

function isOverridden(id: string, scope: ConfigScopeSnapshot) {
  return scope.scope === 'global' && winningScope(id) === 'workspace';
}

function isCurrent(id: string) {
  return store.currentModel.id === id;
}

/** The draft differs from the saved entry in that scope. */
function isDirty(scope: ConfigScopeSnapshot, id: string) {
  const key = draftKey(scope.scope, id);
  const draft = drafts[key];
  if (!draft) return false;
  const saved = scope.models.find((m) => m.id === id);
  if (!saved) return true;
  return (
    draft.name !== saved.name ||
    String(draft.context_window) !== String(saved.context_window) ||
    draft.provider !== saved.provider
  );
}

async function save(scope: ConfigScopeSnapshot, id: string) {
  error.value = '';
  const draft = drafts[draftKey(scope.scope, id)];
  if (!draft) return;
  const window = String(draft.context_window).trim();
  const ok = await store.saveModel(scope.scope, {
    id,
    name: draft.name.trim(),
    context_window: window === '' ? 0 : window,
    provider: draft.provider.trim(),
  });
  if (ok) {
    savedKeys.add(draftKey(scope.scope, id));
    setTimeout(() => savedKeys.delete(draftKey(scope.scope, id)), 1500);
  } else {
    error.value = store.statusMessage || 'saving the model failed';
  }
}

async function remove(scope: ConfigScopeSnapshot, id: string) {
  error.value = '';
  const ok = await store.deleteModel(scope.scope, id);
  if (!ok) error.value = store.statusMessage || 'removing the model failed';
  delete drafts[draftKey(scope.scope, id)];
}

async function add() {
  error.value = '';
  const id = newModel.id.trim();
  if (!activeScope.value || !id) {
    error.value = 'the model id is required';
    return;
  }
  const window = String(newModel.context_window).trim();
  const ok = await store.saveModel(tab.value, {
    id,
    name: newModel.name.trim(),
    context_window: window === '' ? 0 : window,
    provider: newModel.provider.trim(),
  });
  if (ok) {
    newModel.id = '';
    newModel.name = '';
    newModel.context_window = '';
    newModel.provider = '';
  } else {
    error.value = store.statusMessage || 'adding the model failed';
  }
}

function currentLabel(id: string, name: string, provider: string): string {
  const label = name && name !== id ? `${id} (${name})` : id;
  return provider ? `${label} · ${provider}` : label;
}
</script>

<template>
  <div class="page">
    <div class="content">
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

      <div class="effective">
        <span class="effective-label">effective models</span>
        <span v-if="!store.models.length" class="tag warn">none</span>
        <span v-for="m in store.models" :key="m.id" class="chip" :class="{ current: isCurrent(m.id) }">
          {{ currentLabel(m.id, m.name, m.provider_name) }}
          <span v-if="isCurrent(m.id)" class="tag ok">current</span>
        </span>
      </div>

      <template v-if="activeScope">
        <div class="path">{{ activeScope.path }}</div>

        <div class="tier-head">
          <span class="effective-label">tier mapping · [agent.models]</span>
          <span class="tier-hint">used by subagents with a `model` tier</span>
        </div>

        <div v-for="key in TIER_KEYS" :key="key" class="row tier">
          <span class="id" :title="key">{{ tierLabel(key) }}</span>
          <input
            v-model="tierDrafts[tierDraftKey(tab, key)]"
            class="value"
            :placeholder="key === 'agent.models.efficient' ? 'model id or name (empty = falls back)' : 'model id or name'"
            spellcheck="false"
            @keydown.enter="saveTier(key)"
          />
          <button
            class="btn"
            :class="{ dirty: tierDirty(key), saved: tierSavedKeys.has(tierDraftKey(tab, key)) }"
            @click="saveTier(key)"
          >
            {{ tierSavedKeys.has(tierDraftKey(tab, key)) ? 'saved ✓' : 'save' }}
          </button>
          <span class="tier-effective">
            effective: {{ tierEffectiveText(key) }}
            <span class="tag">{{ tierWinningScope(key) }}</span>
          </span>
        </div>

        <div v-if="!activeScope.models.length" class="empty">
          no [[models]] entries in this scope
        </div>

        <div v-for="model in activeScope.models" :key="model.id" class="row">
          <span class="id" :title="model.id">
            {{ model.id }}
            <span v-if="isCurrent(model.id)" class="tag ok">current</span>
            <span v-if="isOverridden(model.id, activeScope)" class="tag warn">
              effective (this scope)
            </span>
            <span v-if="activeScope.scope === 'global' && winningScope(model.id) === 'workspace'" class="tag">
              overridden by workspace
            </span>
          </span>

          <input
            v-model="draftOf(activeScope, model.id).name"
            class="value"
            placeholder="name (falls back to id)"
            spellcheck="false"
          />
          <input
            v-model="draftOf(activeScope, model.id).context_window"
            class="value window"
            placeholder="context window, e.g. 128k"
            spellcheck="false"
            @keydown.enter="save(activeScope, model.id)"
          />
          <input
            v-model="draftOf(activeScope, model.id).provider"
            class="value provider"
            list="provider-names"
            placeholder="provider"
            spellcheck="false"
            @keydown.enter="save(activeScope, model.id)"
          />

          <button
            class="btn"
            :class="{ dirty: isDirty(activeScope, model.id), saved: savedKeys.has(draftKey(activeScope.scope, model.id)) }"
            @click="save(activeScope, model.id)"
          >
            {{ savedKeys.has(draftKey(activeScope.scope, model.id)) ? 'saved ✓' : 'save' }}
          </button>
          <button class="btn danger" title="remove this [[models]] entry" @click="remove(activeScope, model.id)">
            delete
          </button>
        </div>

        <div class="add">
          <input v-model="newModel.id" class="value" placeholder="new model id" spellcheck="false" />
          <input v-model="newModel.name" class="value" placeholder="name (optional)" spellcheck="false" />
          <input v-model="newModel.context_window" class="value window" placeholder="context window, e.g. 128k" spellcheck="false" />
          <input v-model="newModel.provider" class="value provider" list="provider-names" placeholder="provider" spellcheck="false" @keydown.enter="add" />
          <button class="btn primary" @click="add">+ add model</button>
        </div>

        <datalist id="provider-names">
          <option v-for="name in providerNames" :key="name" :value="name" />
        </datalist>

        <div v-if="error" class="error">{{ error }}</div>
        <div class="hint">
          {{ activeScope.scope === 'workspace' ? 'saved entries override the global config (matched by id)' : 'shared by all workspaces; workspace entries with the same id win' }}
          · the id is the entry key: to rename, delete the entry and add it again
          · context window accepts a token count or a value like `128k`
          · the tier mapping accepts a `[[models]]` id or display name, and also supports `/model set-performance` / `set-efficient`
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.page {
  flex: 1;
  display: flex;
  justify-content: center;
  overflow-y: auto;
  padding: 16px 20px;
}

.content {
  width: 860px;
  max-width: 100%;
  display: flex;
  flex-direction: column;
  gap: 10px;
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

.tag.warn {
  color: var(--warn);
}

.effective {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 6px;
}

.effective-label {
  color: var(--fg-dim);
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.06em;
}

.chip {
  font-size: 12px;
  color: var(--fg);
  border: 1px solid var(--border);
  border-radius: 10px;
  padding: 1px 10px;
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.chip.current {
  border-color: var(--accent);
}

.path {
  color: var(--fg-dim);
  font-size: 11px;
  word-break: break-all;
}

.tier-head {
  display: flex;
  align-items: baseline;
  gap: 10px;
}

.tier-hint {
  color: var(--fg-dim);
  font-size: 11px;
}

.row.tier .id {
  width: 90px;
}

.tier-effective {
  color: var(--fg-dim);
  font-size: 11px;
  flex-shrink: 0;
  display: inline-flex;
  align-items: center;
  gap: 6px;
  max-width: 280px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.empty {
  color: var(--fg-dim);
  font-size: 12px;
}

.row,
.add {
  display: flex;
  align-items: center;
  gap: 8px;
}

.id {
  width: 180px;
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

.window {
  flex: 0 0 150px;
}

.provider {
  flex: 0 0 140px;
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

.add {
  margin-top: 6px;
  padding-top: 10px;
  border-top: 1px dashed var(--border);
}

.error {
  color: var(--err);
  font-size: 12px;
}

.hint {
  color: var(--fg-dim);
  font-size: 11px;
}

/* Narrow viewports: the one-line entry becomes a stacked form. */
@media (max-width: 860px) {
  .row,
  .add {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .id {
    width: auto;
    white-space: normal;
    overflow: visible;
    text-overflow: clip;
    flex-wrap: wrap;
  }

  .tier-effective {
    max-width: none;
    white-space: normal;
  }

  .window,
  .provider {
    flex-basis: auto;
  }

  .add .btn.primary {
    grid-column: 1 / -1;
    justify-self: start;
  }
}

@media (max-width: 560px) {
  .row,
  .add {
    grid-template-columns: 1fr;
  }

  .page {
    padding: 12px;
  }
}
</style>
