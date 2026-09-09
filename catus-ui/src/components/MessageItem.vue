<script setup lang="ts">
import { computed, ref } from 'vue';
import type { Message, ToolCall } from '../types';

const props = defineProps<{
  message: Message;
  messages: Message[];
  live?: boolean;
}>();

const kind = computed(() => props.message.role);

const reasoningOpen = ref(false);

/** Tool names backing this message's tool results / calls. */
function toolNameFor(callId: string): string {
  for (const m of props.messages) {
    const call = (m.tool_calls ?? []).find((c) => c.id === callId);
    if (call) return call.function.name;
  }
  return 'tool';
}

function toolCalls(message: Message): ToolCall[] {
  return message.tool_calls ?? [];
}
</script>

<template>
  <!-- User -->
  <div v-if="kind === 'user'" class="msg user">
    <div class="label">you</div>
    <pre>{{ message.content }}</pre>
  </div>

  <!-- Assistant -->
  <div v-else-if="kind === 'assistant'" class="msg assistant" :class="{ live }">
    <div class="label">
      assistant
      <button
        v-if="message.reasoning_content"
        class="reasoning-toggle"
        @click="reasoningOpen = !reasoningOpen"
      >
        {{ reasoningOpen ? '▾ reasoning' : '▸ reasoning' }}
      </button>
    </div>
    <pre v-if="reasoningOpen" class="reasoning">{{ message.reasoning_content }}</pre>
    <pre v-if="message.content">{{ message.content }}</pre>
    <div v-for="call in toolCalls(message)" :key="call.id" class="tool-call">
      <span class="tool-name">⚙ {{ call.function.name }}</span>
      <pre class="tool-args">{{ call.function.arguments }}</pre>
    </div>
  </div>

  <!-- Tool result -->
  <div v-else-if="kind === 'tool'" class="msg tool">
    <div class="label">⚙ {{ toolNameFor(message.tool_call_id ?? '') }}</div>
    <pre>{{ message.content }}</pre>
  </div>

  <!-- Event notice -->
  <div v-else-if="kind === 'event'" class="msg event">
    <pre>{{ message.content }}</pre>
  </div>
</template>

<style scoped>
.msg {
  margin-bottom: 14px;
  border-radius: var(--radius);
  padding: 8px 12px;
  max-width: 100%;
}

.label {
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.08em;
  color: var(--fg-dim);
  margin-bottom: 4px;
  display: flex;
  gap: 10px;
  align-items: center;
}

.user {
  background: var(--bg-elev);
  border-left: 3px solid var(--accent);
}

.assistant {
  border-left: 3px solid var(--ok);
}

.assistant.live .label::after {
  content: '●';
  color: var(--ok);
  animation: blink 1s infinite;
}

@keyframes blink {
  50% {
    opacity: 0.2;
  }
}

.reasoning-toggle {
  border: none;
  background: none;
  padding: 0;
  color: var(--fg-dim);
  font-size: 11px;
}

.reasoning {
  color: var(--fg-dim);
  font-size: 13px;
  border-left: 2px solid var(--border);
  padding-left: 10px;
  margin-bottom: 6px;
}

.tool {
  border-left: 3px solid var(--warn);
  background: var(--bg-elev);
}

.tool pre {
  color: var(--fg-dim);
  font-size: 13px;
}

.tool-call {
  margin-top: 8px;
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 6px 10px;
}

.tool-name {
  color: var(--warn);
  font-size: 13px;
}

.tool-args {
  color: var(--fg-dim);
  font-size: 12px;
  margin-top: 2px;
}

.event {
  color: var(--fg-dim);
  font-style: italic;
}
</style>
