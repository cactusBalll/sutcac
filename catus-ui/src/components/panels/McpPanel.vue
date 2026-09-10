<script setup lang="ts">
import { useRuntimeStore } from '../../stores/runtime';

const store = useRuntimeStore();
</script>

<template>
  <div class="page">
    <div v-if="!store.mcpServers.length" class="empty">no MCP servers configured</div>
    <div v-for="server in store.mcpServers" :key="server.name" class="server">
      <div class="row">
        <span class="name">{{ server.name }}</span>
        <span class="state" :class="{ ok: server.connected && server.enabled, dim: !server.connected }">
          {{ server.connected ? 'connected' : 'not connected' }}
          <template v-if="!server.enabled"> · disabled</template>
        </span>
        <div class="actions">
          <button
            v-if="server.enabled && !server.connected"
            class="connect"
            title="connect to this server now"
            @click="store.connectMcp(server.name)"
          >
            connect
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

.dim {
  color: var(--fg-dim);
  font-size: 11px;
}

.empty {
  color: var(--fg-dim);
  padding: 24px 0;
}
</style>
