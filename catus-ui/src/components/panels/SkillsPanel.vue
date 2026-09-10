<script setup lang="ts">
import { ref } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';
import type { SkillPreview } from '../../types';

const store = useRuntimeStore();
const preview = ref<SkillPreview | null>(null);
const previewError = ref('');
const previewLoading = ref(false);

async function showPreview(name: string) {
  previewLoading.value = true;
  previewError.value = '';
  try {
    preview.value = await store.skillPreview(name);
  } catch (e) {
    preview.value = null;
    previewError.value = String(e);
  } finally {
    previewLoading.value = false;
  }
}
</script>

<template>
  <div class="page">
    <div class="list">
      <div v-if="!store.skillCatalog.length" class="empty">no skills discovered</div>
      <div
        v-for="skill in store.skillCatalog"
        :key="skill.name"
        class="item"
        :class="{ selected: preview?.name === skill.name }"
        @click="showPreview(skill.name)"
      >
        <div class="row">
          <span class="name">{{ skill.name }}</span>
          <button
            class="switch"
            :class="{ off: skill.disabled }"
            :title="skill.disabled ? 'use_skill rejects it; the prompt catalog is unchanged (manual use still works)' : 'enabled'"
            @click="store.toggleSkill(skill.name)"
          >
            {{ skill.disabled ? 'disabled' : 'enabled' }}
          </button>
        </div>
        <div class="desc">{{ skill.description }}</div>
      </div>
    </div>
    <div class="preview">
      <div v-if="previewLoading" class="empty">loading…</div>
      <div v-else-if="previewError" class="empty error">{{ previewError }}</div>
      <template v-else-if="preview">
        <div class="preview-head">
          {{ preview.name }}
        </div>
        <pre class="content">{{ preview.content }}</pre>
      </template>
      <div v-else class="empty">select a skill to preview its SKILL.md</div>
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

.item {
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 8px 12px;
  cursor: pointer;
}

.item:hover,
.item.selected {
  border-color: var(--accent);
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

.desc {
  color: var(--fg-dim);
  font-size: 12px;
  margin-top: 2px;
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

.preview {
  flex: 1;
  overflow-y: auto;
  padding: 16px 20px;
  min-width: 0;
}

.preview-head {
  color: var(--accent);
  font-weight: bold;
  margin-bottom: 10px;
}

.content {
  font-size: 12px;
  color: var(--fg);
}

.empty {
  color: var(--fg-dim);
  padding: 24px 0;
}

.empty.error {
  color: var(--err);
}
</style>
