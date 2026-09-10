<script setup lang="ts">
import { onMounted, onUnmounted } from 'vue';
import { useRuntimeStore } from '../../stores/runtime';
import AskOverlay from './AskOverlay.vue';
import ListPicker from './ListPicker.vue';
import StatusOverlay from './StatusOverlay.vue';
import ConfigOverlay from './ConfigOverlay.vue';
import WatchSubagentOverlay from './WatchSubagentOverlay.vue';

const store = useRuntimeStore();

function onKeydown(e: KeyboardEvent) {
  if (!store.overlay) return;
  if (e.key === 'Escape') {
    e.preventDefault();
    if (store.overlay.kind === 'ask') {
      store.cancelInteraction();
    } else {
      store.closeOverlay();
    }
    return;
  }
  // Let buttons/inputs handle Enter themselves; only swallow it when the
  // focus is on a passive element so a stray Enter cannot re-trigger
  // whatever opened the overlay.
  if (e.key === 'Enter') {
    const target = e.target as HTMLElement | null;
    const interactive = target?.closest('button, input, textarea, select, a[href]');
    if (!interactive) e.preventDefault();
  }
}

function onBackdropClick() {
  if (!store.overlay) return;
  if (store.overlay.kind === 'ask') {
    store.cancelInteraction();
  } else {
    store.closeOverlay();
  }
}

onMounted(() => window.addEventListener('keydown', onKeydown));
onUnmounted(() => window.removeEventListener('keydown', onKeydown));
</script>

<template>
  <div v-if="store.overlay" class="backdrop" @click.self="onBackdropClick">
    <AskOverlay v-if="store.overlay.kind === 'ask'" :questions="store.overlay.questions" />
    <template v-else-if="store.overlay.kind === 'ui'">
      <StatusOverlay v-if="store.overlay.request.page === 'show_status'" />
      <ConfigOverlay v-else-if="store.overlay.request.page === 'show_config'" />
      <WatchSubagentOverlay v-else-if="store.overlay.request.page === 'watch_subagent'" />
      <ListPicker
        v-else-if="store.overlay.request.page === 'show_resume_picker'"
        title="resume session"
        :items="store.overlay.request.items"
        action="resume_history"
      />
      <ListPicker
        v-else-if="store.overlay.request.page === 'show_skills'"
        title="activate skill"
        :items="store.overlay.request.items"
        action="activate_skill"
      />
      <ListPicker
        v-else-if="store.overlay.request.page === 'show_models'"
        title="switch model"
        :items="store.overlay.request.items"
        :selected="store.overlay.request.selected"
        action="switch_model"
      />
      <ListPicker
        v-else-if="store.overlay.request.page === 'show_agents'"
        title="dispatch agent"
        :items="store.overlay.request.items"
        action="prefill_agent"
      />
      <ListPicker
        v-else-if="store.overlay.request.page === 'show_subagents'"
        title="watch subagent"
        :items="store.overlay.request.items"
        action="watch_subagent"
      />
      <ListPicker
        v-else-if="store.overlay.request.page === 'close_subagent_picker'"
        title="close subagent"
        :items="store.overlay.request.items"
        action="close_subagent"
      />
    </template>
  </div>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  background: rgba(6, 8, 12, 0.72);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 50;
}

/* Small screens: panels fill the viewport instead of floating centered. */
@media (max-width: 560px) {
  .backdrop {
    align-items: stretch;
    padding: 12px;
  }
}
</style>
