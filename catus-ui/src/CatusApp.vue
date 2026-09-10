<script setup lang="ts">
import { onMounted } from 'vue';
import type { CatusTransport } from './transport';
import { useRuntimeStore } from './stores/runtime';
import Sidebar from './components/Sidebar.vue';
import ChatView from './components/ChatView.vue';
import InputLine from './components/InputLine.vue';
import StatusBar from './components/StatusBar.vue';
import OverlayHost from './components/overlays/OverlayHost.vue';
import PanelHost from './components/panels/PanelHost.vue';

const props = defineProps<{ transport: CatusTransport }>();
const store = useRuntimeStore();

onMounted(() => {
  store.init(props.transport);
});
</script>

<template>
  <div v-if="store.startupError" class="fatal">
    <h2>startup failed</h2>
    <pre>{{ store.startupError }}</pre>
  </div>
  <div v-else-if="store.quitRequested" class="fatal">
    <h2>session ended</h2>
    <p>the catus session has been closed</p>
  </div>
  <div v-else-if="!store.started" class="fatal"><p>loading session…</p></div>
  <div v-else class="app">
    <Sidebar />
    <div class="main">
      <ChatView />
      <InputLine />
      <StatusBar />
    </div>
    <OverlayHost />
    <PanelHost />
  </div>
</template>

<style scoped>
.app {
  display: flex;
  height: 100%;
}

.main {
  flex: 1;
  display: flex;
  flex-direction: column;
  min-width: 0;
}

.fatal {
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 12px;
  color: var(--err);
  padding: 32px;
}
</style>
