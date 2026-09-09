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

function onWheel(e: WheelEvent) {
  const el = scroller.value;
  if (!el) return;
  const atBottom = el.scrollHeight - el.scrollTop - el.clientHeight < 40;
  store.autoScroll = atBottom && e.deltaY >= 0;
}
</script>

<template>
  <div ref="scroller" class="chat" @wheel="onWheel">
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
</template>

<style scoped>
.chat {
  flex: 1;
  overflow-y: auto;
  padding: 16px 20px;
}

.pending-tool {
  color: var(--warn);
  font-size: 13px;
}
</style>
