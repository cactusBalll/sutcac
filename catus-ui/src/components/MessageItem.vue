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
const toolOpen = ref(false);
/** Bumped while a copy action shows its "copied" confirmation. */
const copiedKey = ref('');

/** Tool results longer than this collapse to a preview. */
const COLLAPSE_LINES = 12;
const PREVIEW_LINES = 5;

const toolLines = computed(() => props.message.content.split('\n').length);
const toolCollapsible = computed(() => kind.value === 'tool' && toolLines.value > COLLAPSE_LINES);
const toolContent = computed(() => {
  if (!toolCollapsible.value || toolOpen.value) return props.message.content;
  return props.message.content.split('\n').slice(0, PREVIEW_LINES).join('\n');
});

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

async function copy(key: string, text: string) {
  try {
    await navigator.clipboard.writeText(text);
    copiedKey.value = key;
    setTimeout(() => {
      if (copiedKey.value === key) copiedKey.value = '';
    }, 1500);
  } catch {
    // Clipboard access may be denied (e.g. insecure context); stay silent.
  }
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
      <div class="tool-call-head">
        <span class="tool-name">⚙ {{ call.function.name }}</span>
        <button class="copy" @click="copy(call.id, call.function.arguments)">
          {{ copiedKey === call.id ? 'copied' : 'copy' }}
        </button>
      </div>
      <pre class="tool-args">{{ call.function.arguments }}</pre>
    </div>
  </div>

  <!-- Tool result -->
  <div v-else-if="kind === 'tool'" class="msg tool">
    <div class="label">
      <span>⚙ {{ toolNameFor(message.tool_call_id ?? '') }}</span>
      <span class="spacer" />
      <button
        v-if="toolCollapsible"
        class="toggle"
        @click="toolOpen = !toolOpen"
      >
        {{ toolOpen ? `▾ collapse` : `▸ ${toolLines} lines` }}
      </button>
      <button class="copy" @click="copy(`result:${message.tool_call_id}`, message.content)">
        {{ copiedKey === `result:${message.tool_call_id}` ? 'copied' : 'copy' }}
      </button>
    </div>
    <pre>{{ toolContent }}</pre>
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

.spacer {
  flex: 1;
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
  max-height: 60vh;
  overflow-y: auto;
}

.tool-call {
  margin-top: 8px;
  background: var(--bg-input);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 6px 10px;
}

.tool-call-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
}

.tool-name {
  color: var(--warn);
  font-size: 13px;
}

.tool-args {
  color: var(--fg-dim);
  font-size: 12px;
  margin-top: 2px;
  max-height: 40vh;
  overflow-y: auto;
}

.copy,
.toggle {
  border: none;
  background: none;
  padding: 0;
  color: var(--fg-dim);
  font-size: 10px;
  text-transform: none;
  letter-spacing: normal;
}

.copy:hover,
.toggle:hover {
  color: var(--accent);
}

.event {
  color: var(--fg-dim);
  font-style: italic;
}
</style>
