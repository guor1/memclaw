<script setup>
import { ref, nextTick } from 'vue'

/**
 * Dual-purpose modal:
 * - kind="approval": server asks to run a command → allow / deny
 * - kind="input":    ask_user tool asks a free-text question → submit / cancel
 */
const props = defineProps({
  kind: { type: String, required: true },
  summary: { type: String, default: '' },
  command: { type: String, default: '' },
  prompt: { type: String, default: '' },
})

const emit = defineEmits(['allow', 'deny', 'submit', 'cancel'])

const answer = ref('')
const dialogEl = ref(null)

// Focus the primary control on mount so keyboard users land in the right place.
nextTick(() => {
  const target = props.kind === 'input'
    ? dialogEl.value?.querySelector('textarea')
    : dialogEl.value?.querySelector('.primary')
  target?.focus()
})

function onKeydown(e) {
  if (e.key === 'Escape') {
    e.stopPropagation()
    props.kind === 'approval' ? emit('deny') : emit('cancel')
  }
  // Ctrl/Cmd+Enter submits the free-text form.
  if (props.kind === 'input' && e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
    e.preventDefault()
    emit('submit', answer.value)
  }
}
</script>

<template>
  <div class="backdrop" role="presentation" @keydown="onKeydown">
    <div
      ref="dialogEl"
      class="dialog"
      role="dialog"
      aria-modal="true"
      aria-labelledby="dlg-title"
    >
      <template v-if="kind === 'approval'">
        <h2 id="dlg-title" class="title">需要你确认</h2>
        <p class="summary">{{ summary }}</p>
        <pre v-if="command" class="command"><code>{{ command }}</code></pre>
        <div class="actions">
          <button class="btn ghost" @click="emit('deny')">拒绝</button>
          <button class="btn primary" @click="emit('allow')">允许</button>
        </div>
      </template>
      <template v-else>
        <h2 id="dlg-title" class="title">助手有个问题</h2>
        <p class="summary">{{ prompt }}</p>
        <textarea
          v-model="answer"
          rows="3"
          placeholder="你的回答…（Ctrl+Enter 提交）"
          aria-label="回答"
        ></textarea>
        <div class="actions">
          <button class="btn ghost" @click="emit('cancel')">跳过</button>
          <button class="btn primary" @click="emit('submit', answer)" :disabled="!answer.trim()">
            提交
          </button>
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.backdrop {
  position: fixed;
  inset: 0;
  background: color-mix(in srgb, var(--n-9) 55%, transparent);
  display: grid;
  place-items: center;
  padding: var(--sp-4);
  z-index: 100;
  animation: fade 140ms ease;
}

@keyframes fade { from { opacity: 0; } }

.dialog {
  background: var(--surface-raised);
  border: 1px solid var(--border);
  border-radius: var(--r-md);
  padding: var(--sp-5);
  width: min(30rem, 100%);
  box-shadow: 0 12px 32px color-mix(in srgb, var(--n-9) 24%, transparent);
  animation: rise 160ms cubic-bezier(0.2, 0.8, 0.3, 1);
}

@keyframes rise {
  from { transform: translateY(6px) scale(0.99); opacity: 0; }
}

@media (prefers-reduced-motion: reduce) {
  .backdrop, .dialog { animation: none; }
}

.title {
  font-size: 15px;
  font-weight: 600;
  margin-bottom: var(--sp-2);
}

.summary {
  color: var(--text-muted);
  font-size: 13px;
  margin-bottom: var(--sp-3);
  white-space: pre-wrap;
}

.command {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--r-sm);
  padding: var(--sp-2) var(--sp-3);
  margin-bottom: var(--sp-4);
  overflow-x: auto;
}

.command code {
  font-family: ui-monospace, "Cascadia Code", "Fira Code", monospace;
  font-size: 12px;
  color: var(--text);
}

textarea {
  width: 100%;
  font: inherit;
  color: var(--text);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--r-md);
  padding: var(--sp-2) var(--sp-3);
  resize: vertical;
  margin-bottom: var(--sp-4);
}

.actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--sp-2);
}

.btn {
  padding: var(--sp-2) var(--sp-4);
  border-radius: var(--r-md);
  font-size: 13px;
  font-weight: 500;
  transition: background 120ms ease, opacity 120ms ease;
}

.primary {
  background: var(--accent);
  color: white;
}

.primary:hover:not(:disabled) { background: var(--accent-hover); }

.primary:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}

.ghost {
  background: var(--surface);
  border: 1px solid var(--border);
  color: var(--text);
}

.ghost:hover { background: var(--border); }
</style>
