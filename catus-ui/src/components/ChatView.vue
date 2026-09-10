<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue';
import { useRuntimeStore } from '../stores/runtime';
import MessageItem from './MessageItem.vue';
import type { Message } from '../types';

const store = useRuntimeStore();
const scroller = ref<HTMLElement | null>(null);

const streamingMessage = computed<Message>(() => ({
  role: 'assistant',
  content: store.streamingText,
  reasoning_content: store.streamingReasoning,
}));

const isEmpty = computed(
  () =>
    !store.visibleMessages.length &&
    !store.streamingText &&
    !store.streamingReasoning &&
    !store.pendingToolCalls.length,
);

function atBottom(): boolean {
  const el = scroller.value;
  if (!el) return true;
  return el.scrollHeight - el.scrollTop - el.clientHeight < 40;
}

function scrollToBottom() {
  if (!store.autoScroll) return;
  nextTick(() => {
    const el = scroller.value;
    if (el) el.scrollTop = el.scrollHeight;
  });
}

watch(
  () => [
    store.messages.length,
    store.visibleMessages[store.visibleMessages.length - 1]?.content,
    store.streamingText,
    store.streamingReasoning,
    store.pendingToolCalls.length,
  ],
  scrollToBottom,
  { flush: 'post' },
);

// Any scroll gesture (wheel, scrollbar drag, keyboard, touch) re-evaluates
// whether the view is pinned to the bottom.
function onScroll() {
  store.autoScroll = atBottom();
}

function resumeFollowing() {
  store.autoScroll = true;
  scrollToBottom();
}
</script>

<template>
  <div class="chat-wrap">
    <div ref="scroller" class="chat" @scroll.passive="onScroll">
      <div class="thread">
        <div v-if="isEmpty" class="empty">
          <p>send a message to start</p>
          <p class="dim">/command for slash commands · @agent to dispatch a subagent · /help for help</p>
        </div>
        <MessageItem
          v-for="(message, index) in store.visibleMessages"
          :key="index"
          :message="message"
          :messages="store.messages"
        />
        <MessageItem
          v-if="store.streamingText || store.streamingReasoning"
          :message="streamingMessage"
          :messages="store.messages"
          live
        />
        <div v-if="store.pendingToolCalls.length" class="pending-tools">
          <div v-for="call in store.pendingToolCalls" :key="call.id" class="pending-tool">
            ⚙ running {{ call.name }}…
          </div>
        </div>
      </div>
    </div>
    <button
      v-if="!store.autoScroll && !isEmpty"
      class="to-bottom"
      title="jump to the latest message"
      @click="resumeFollowing"
    >
      ↓ new messages
    </button>
  </div>
</template>

<style scoped>
.chat-wrap {
  position: relative;
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
}

.chat {
  flex: 1;
  overflow-y: auto;
  padding: 16px 20px;
}

/* Long lines stay readable on very wide screens. */
.thread {
  max-width: 980px;
  width: 100%;
  margin: 0 auto;
}

.empty {
  height: 100%;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 4px;
  color: var(--fg-dim);
}

.empty p {
  margin: 0;
}

.empty .dim {
  font-size: 12px;
  opacity: 0.7;
}

.pending-tool {
  color: var(--warn);
  font-size: 13px;
}

.to-bottom {
  position: absolute;
  right: 24px;
  bottom: 16px;
  font-size: 12px;
  padding: 3px 12px;
  border-radius: 12px;
  background: var(--bg-elev);
  color: var(--accent);
  border: 1px solid var(--accent);
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.4);
}

@media (max-width: 560px) {
  .chat {
    padding: 8px 12px;
  }

  .to-bottom {
    right: 12px;
  }
}
</style>
