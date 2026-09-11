<script setup>
import { computed } from 'vue'
import ToolCard from './ToolCard.vue'
import { renderMarkdown } from '../lib/markdown.js'

/** A single message bubble — user, assistant, or tool. */
const props = defineProps({
  msg: { type: Object, required: true },
})

// Tool calls render as structured ToolCard; assistant prose renders as sanitized
// Markdown (marked → DOMPurify). User text stays plain (no markdown interpretation
// for the user's own words, matching typical chat UIs).
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

// Assistant content rendered to sanitized HTML. The streaming caret ▍ is
// appended outside the markdown so it never gets swallowed by a half-open fence.
const assistantHtml = computed(() => renderMarkdown(props.msg.content ?? ''))
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
      <div v-else-if="isAssistant" class="message-markdown" v-html="assistantHtml"></div>
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
  min-width: 0;
  max-width: min(72ch, 92%);
  padding: var(--sp-2) var(--sp-3);
  border-radius: var(--r-md);
  font-size: 14px;
}

.bubble.user {
  max-width: min(60ch, 80%);
  background: var(--accent);
  color: white;
  border-bottom-right-radius: var(--r-sm);
}

.bubble.assistant {
  /* Assistant prose gets a generous reading column, not the narrow 72ch default.
     min() keeps it fluid: 840px on wide panes, shrinking to the full messages
     column (minus side padding) on narrow ones. Shrink-to-fit still keeps short
     replies compact. */
  max-width: min(840px, 100%);
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

/* ── Assistant Markdown body ──
   marked emits raw HTML elements inside .message-markdown; scoped styles can't
   reach them via `scoped` attribute, so these rules target the wrapper's
   descendants. Everything here is post-DOMPurify, so no untrusted nodes. */
.message-markdown {
  font: inherit;
  line-height: 1.6;
  word-break: break-word;
}

.message-markdown :deep(p) {
  margin: 0 0 var(--sp-2);
}
.message-markdown :deep(p:last-child) {
  margin-bottom: 0;
}
.message-markdown :deep(ul),
.message-markdown :deep(ol) {
  margin: 0 0 var(--sp-2);
  padding-left: var(--sp-5);
}
.message-markdown :deep(li) {
  margin: var(--sp-1) 0;
}
.message-markdown :deep(h1),
.message-markdown :deep(h2),
.message-markdown :deep(h3),
.message-markdown :deep(h4) {
  margin: var(--sp-3) 0 var(--sp-2);
  font-weight: 600;
  line-height: 1.3;
}
.message-markdown :deep(h1) { font-size: 1.3em; }
.message-markdown :deep(h2) { font-size: 1.2em; }
.message-markdown :deep(h3) { font-size: 1.1em; }
.message-markdown :deep(h4) { font-size: 1em; }
.message-markdown :deep(blockquote) {
  margin: 0 0 var(--sp-2);
  padding: var(--sp-1) var(--sp-3);
  border-left: 3px solid var(--border);
  color: var(--text-muted);
}
.message-markdown :deep(code) {
  font-family: ui-monospace, "Cascadia Code", "Fira Code", monospace;
  font-size: 0.9em;
  background: color-mix(in srgb, var(--n-2) 55%, transparent);
  border-radius: var(--r-sm);
  padding: 1px 5px;
}
.message-markdown :deep(pre) {
  margin: 0 0 var(--sp-2);
  padding: var(--sp-3);
  background: color-mix(in srgb, var(--n-2) 55%, transparent);
  border: 1px solid var(--border);
  border-radius: var(--r-sm);
  overflow-x: auto;
}
.message-markdown :deep(pre code) {
  background: none;
  padding: 0;
  font-size: 0.85em;
  line-height: 1.5;
}
.message-markdown :deep(a) {
  color: var(--accent);
  text-decoration: underline;
  text-underline-offset: 2px;
}
.message-markdown :deep(a:hover) {
  color: var(--accent-hover);
}
.message-markdown :deep(hr) {
  margin: var(--sp-3) 0;
  border: 0;
  border-top: 1px solid var(--border);
}
.message-markdown :deep(table) {
  border-collapse: collapse;
  margin: 0 0 var(--sp-2);
  font-size: 0.9em;
}
.message-markdown :deep(th),
.message-markdown :deep(td) {
  border: 1px solid var(--border);
  padding: var(--sp-1) var(--sp-2);
  text-align: left;
}
.message-markdown :deep(th) {
  background: color-mix(in srgb, var(--n-2) 45%, transparent);
  font-weight: 600;
}
.message-markdown :deep(img) {
  max-width: 100%;
  border-radius: var(--r-sm);
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
