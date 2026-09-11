<script setup>
import { computed } from 'vue'
import ToolCard from './ToolCard.vue'

/** A single message bubble — user, assistant, or tool. */
const props = defineProps({
  msg: { type: Object, required: true },
})

// Sanitize content: we don't render arbitrary HTML. Assistant/user text is plain
// text; tool calls render as structured ToolCard (no HTML injection).
const isUser      = computed(() => props.msg.role === 'user')
const isAssistant = computed(() => props.msg.role === 'assistant')
const isTool      = computed(() => props.msg.role === 'tool')
const isSystem    = computed(() => props.msg.role === 'system')

// A structured tool call (from live events or history pairing) has a `name`.
// Legacy bare tool messages (no structure) fall back to plain text.
const isStructuredTool = computed(() => isTool.value && Boolean(props.msg.name))

// Empty assistant bubbles are dispatch-only records (they carry tool_calls but
// no prose). Never render a blank bubble for them — the paired ToolCard conveys
// the action. Assistant prose that's still streaming (pending, empty so far) is
// also suppressed until it has content.
const showAssistant = computed(() => isAssistant.value && (props.msg.content ?? '').trim() !== '')
</script>

<template>
  <div v-if="isSystem" class="system-msg" role="note">
    {{ msg.content }}
  </div>

  <!-- Structured tool call → ToolCard -->
  <div v-else-if="isStructuredTool" class="tool-row-wrap">
    <ToolCard :tool="msg" />
  </div>

  <div v-else-if="showAssistant || isUser || isTool" class="bubble-row" :class="{ user: isUser, tool: isTool }">
    <div
      class="bubble"
      :class="{ user: isUser, assistant: isAssistant, 'tool-bubble': isTool, pending: msg.pending }"
      :aria-label="isUser ? '你' : isAssistant ? '助手' : '工具'"
    >
      <template v-if="isTool">
        <span class="tool-icon" aria-hidden="true">
          {{ toolIcon[msg.toolStatus] ?? '⚙️' }}
        </span>
        <code class="tool-call">{{ msg.content }}</code>
        <span v-if="msg.toolStatus === 'running'" class="spinner" aria-label="执行中"></span>
      </template>
      <pre v-else class="message-text">{{ msg.content }}{{ msg.pending ? '▍' : '' }}</pre>
    </div>
  </div>
</template>

<script>
const toolIcon = {
  running: '⚙️',
  ok:      '✓',
  error:   '✗',
  aborted: '⊘',
}
</script>

<style scoped>
.bubble-row {
  display: flex;
  justify-content: flex-start;   /* assistant: left */
}

.bubble-row.user {
  justify-content: flex-end;
}

.bubble-row.tool {
  justify-content: center;
}

.bubble {
  max-width: min(72ch, 92%);
  padding: var(--sp-2) var(--sp-3);
  border-radius: var(--r-md);
  font-size: 14px;
}

.bubble.user {
  background: var(--accent);
  color: white;
  border-bottom-right-radius: var(--r-sm);
}

.bubble.assistant {
  background: var(--surface-raised);
  border: 1px solid var(--border);
  border-bottom-left-radius: var(--r-sm);
}

.bubble.pending {
  /* streaming caret already injected into text */
}

.bubble.tool-bubble {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  background: color-mix(in srgb, var(--n-2) 40%, transparent);
  border: 1px dashed var(--border);
  border-radius: var(--r-sm);
  padding: var(--sp-1) var(--sp-3);
  font-size: 12px;
  color: var(--text-muted);
  max-width: 100%;
}

.tool-icon { font-style: normal; }

.tool-call {
  font-family: ui-monospace, "Cascadia Code", "Fira Code", monospace;
  font-size: 12px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text);
}

.message-text {
  font: inherit;
  white-space: pre-wrap;
  word-break: break-word;
  margin: 0;
}

/* Structured tool rows sit left-aligned under the assistant column, full width. */
.tool-row-wrap {
  display: flex;
  justify-content: flex-start;
  max-width: min(72ch, 100%);
}

.spinner {
  display: inline-block;
  width: 12px;
  height: 12px;
  border: 2px solid color-mix(in srgb, var(--accent) 30%, transparent);
  border-top-color: var(--accent);
  border-radius: 50%;
  animation: spin 0.8s linear infinite;
  flex-shrink: 0;
}

@keyframes spin { to { transform: rotate(360deg); } }

@media (prefers-reduced-motion: reduce) {
  .spinner { animation: none; }
}

.system-msg {
  text-align: center;
  color: var(--text-muted);
  font-size: 12px;
  padding: var(--sp-1) 0;
  border-top: 1px solid var(--border);
  border-bottom: 1px solid var(--border);
}
</style>
