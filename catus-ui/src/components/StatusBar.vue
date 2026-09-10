<script setup lang="ts">
import { computed } from 'vue';
import { useRuntimeStore } from '../stores/runtime';

const store = useRuntimeStore();

const statusClass = computed(() => store.status);

const ctxTokens = computed(() => store.usage.prompt_tokens.toLocaleString());
const totalTokens = computed(() => store.usage.total_tokens.toLocaleString());

const ctxPercent = computed(() => {
  const window = store.currentModel.context_window;
  if (!window || window <= 0) return null;
  return Math.min(100, (store.usage.prompt_tokens / window) * 100);
});

const ctxBarClass = computed(() => {
  const pct = ctxPercent.value;
  if (pct === null) return '';
  if (pct >= 90) return 'critical';
  if (pct >= 75) return 'warn';
  return 'ok';
});

const ctxPercentLabel = computed(() =>
  ctxPercent.value === null ? '' : `${ctxPercent.value.toFixed(1)}%`,
);
</script>

<template>
  <div class="status-bar">
    <div class="left">
      <span class="dot" :class="statusClass" />
      <span v-if="!store.connected" class="disconnected">reconnecting…</span>
      <button class="model" title="switch model" @click="store.openModelPicker()">
        {{ store.currentModel.id || store.currentModel.name }}
      </button>
      <span v-if="store.subagents.length" class="dim">· {{ store.subagents.length }} subagents</span>
      <span v-if="store.memoryAvailable" class="dim">· memory {{ store.memorySessionEnabled ? 'on' : 'off' }}</span>
      <span
        v-if="store.statusMessage"
        class="message"
        :class="{ error: store.status === 'error' }"
        :title="store.statusMessage"
      >
        {{ store.statusMessage }}
      </span>
    </div>
    <div class="right">
      <span
        class="ctx"
        :title="
          ctxPercent === null
            ? 'context window unknown'
            : `context window: ${ctxPercentLabel} (${store.usage.prompt_tokens.toLocaleString()} / ${store.currentModel.context_window.toLocaleString()} tok)`
        "
      >
        <template v-if="ctxPercent !== null">
          <span class="ctx-bar" aria-hidden="true">
            <span class="ctx-bar-fill" :class="ctxBarClass" :style="{ width: `${ctxPercent}%` }" />
          </span>
          <span class="ctx-pct" :class="ctxBarClass">{{ ctxPercentLabel }}</span>
          <span class="sep">|</span>
        </template>
        <span>ctx {{ ctxTokens }} tok</span>
        <span class="sep">|</span>
        <span class="total">total {{ totalTokens }}</span>
      </span>
    </div>
  </div>
</template>

<style scoped>
.status-bar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 16px;
  padding: 5px 20px;
  font-size: 12px;
  color: var(--fg-dim);
  background: var(--bg-elev);
  border-top: 1px solid var(--border);
}

.left {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}

.dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--fg-dim);
  flex-shrink: 0;
}

.dot.streaming,
.dot.running_tool {
  background: var(--ok);
  animation: pulse 1.2s infinite;
}

.dot.error {
  background: var(--err);
}

.disconnected {
  color: var(--warn);
  animation: pulse 1.2s infinite;
}

@keyframes pulse {
  50% {
    opacity: 0.3;
  }
}

.model {
  color: var(--fg);
  background: none;
  border: none;
  padding: 0;
  font: inherit;
  cursor: pointer;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.model:hover {
  color: var(--accent);
  text-decoration: underline;
}

.message {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--accent);
}

.message.error {
  color: var(--err);
}

.dim {
  color: var(--fg-dim);
}

.ctx {
  display: inline-flex;
  align-items: center;
  gap: 6px;
}

.ctx-bar {
  display: inline-block;
  width: 48px;
  height: 6px;
  border-radius: 3px;
  background: var(--border);
  overflow: hidden;
  flex-shrink: 0;
}

.ctx-bar-fill {
  display: block;
  height: 100%;
  border-radius: 3px;
  transition: width 0.3s ease;
}

.ctx-bar-fill.ok {
  background: var(--ok);
}

.ctx-bar-fill.warn {
  background: var(--warn);
}

.ctx-bar-fill.critical {
  background: var(--err);
}

.ctx-pct {
  min-width: 40px;
  text-align: right;
  color: var(--fg);
}

.ctx-pct.warn {
  color: var(--warn);
}

.ctx-pct.critical {
  color: var(--err);
}

.sep {
  color: var(--border);
}

/* Progressive degradation on narrow viewports: drop the secondary counts,
   then the total, and let the model name truncate. */
@media (max-width: 768px) {
  .status-bar {
    padding: 4px 12px;
  }

  .dim {
    display: none;
  }

  .model {
    max-width: 160px;
  }
}

@media (max-width: 560px) {
  .status-bar {
    font-size: 11px;
  }

  .total {
    display: none;
  }

  .ctx-bar {
    width: 36px;
  }

  .ctx-pct {
    min-width: 34px;
  }
}

</style>
