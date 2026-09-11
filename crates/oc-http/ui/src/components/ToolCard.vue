<script setup>
import { computed, ref } from 'vue'
import { resolveToolView, isErrorOutput } from '../lib/toolDisplay.js'

/**
 * A single tool call, rendered openclaw-style: a collapsed one-line summary row
 * (chevron + icon + verb/target + spinner/badge) that expands to a body with
 * command terminal block / key-value args / output block.
 *
 * `tool` shape (see state.js / ChatPane.vue):
 *   { id, name, args, output, status, pending }
 *   status: 'running' | 'ok' | 'error' | 'aborted' | undefined
 */
const props = defineProps({
  tool: { type: Object, required: true },
})

const expanded = ref(false)

const view = computed(() => resolveToolView(props.tool.name, props.tool.args))
const status = computed(() => props.tool.status ?? null)
const isRunning = computed(() => status.value === 'running')
const isError = computed(
  () => status.value === 'error' || status.value === 'aborted' || isErrorOutput(props.tool.output),
)

// Chevron ▸ rotates 90° when open (openclaw .chat-tool-msg-summary::before).
const toggle = () => { expanded.value = !expanded.value }

const argsEntries = computed(() => {
  const rec = view.value.argsRecord
  if (!rec || Array.isArray(rec) || typeof rec !== 'object') return []
  return Object.entries(rec)
})

const outputText = computed(() => (props.tool.output ?? '').trim())

// Value formatting for key-value args (clamped, JSON for nested).
function formatValue(value) {
  if (typeof value === 'string') return clamp(value, 400)
  if (value === null || value === undefined) return String(value)
  if (typeof value === 'number' || typeof value === 'boolean') return String(value)
  try {
    return clamp(JSON.stringify(value), 400)
  } catch {
    return Object.prototype.toString.call(value)
  }
}

function clamp(s, max) {
  const str = String(s)
  return str.length > max ? `${str.slice(0, max)}…` : str
}
</script>

<template>
  <div class="tool-card" :class="{ 'is-error': isError, 'is-running': isRunning }">
    <button
      class="tool-row"
      type="button"
      :aria-expanded="String(expanded)"
      @click="toggle"
    >
      <span class="tool-chevron" aria-hidden="true">▸</span>
      <span class="tool-icon" aria-hidden="true">{{ view.icon }}</span>

      <template v-if="view.kind === 'command'">
        <span class="tool-prompt" aria-hidden="true">$</span>
        <code class="tool-cmd">{{ view.target ?? view.command }}</code>
      </template>
      <template v-else>
        <span class="tool-verb">{{ view.verb }}</span>
        <span v-if="view.target" class="tool-target">{{ view.target }}</span>
        <span v-if="view.targetDetail" class="tool-detail">{{ view.targetDetail }}</span>
      </template>

      <span v-if="isError" class="tool-badge">失败</span>
      <span v-else-if="isRunning" class="tool-spinner" aria-label="执行中"></span>
    </button>

    <div v-if="expanded" class="tool-body">
      <!-- command → terminal block -->
      <div v-if="view.kind === 'command'" class="tool-term" :class="{ 'is-error': isError }">
        <div class="tool-term-cmd">
          <span class="tool-prompt" aria-hidden="true">$</span>
          <code>{{ view.command }}</code>
        </div>
        <pre v-if="outputText" class="tool-term-out"><code>{{ outputText }}</code></pre>
      </div>

      <!-- generic → key-value args + output -->
      <template v-else>
        <div v-if="argsEntries.length" class="tool-kv">
          <div v-for="[k, v] in argsEntries" :key="k" class="tool-kv-row">
            <span class="tool-kv-key">{{ k }}:</span>
            <span class="tool-kv-value">{{ formatValue(v) }}</span>
          </div>
        </div>
        <div v-if="outputText" class="tool-out" :class="{ 'is-error': isError }">
          <span class="tool-out-label">{{ isError ? '错误' : '输出' }}</span>
          <pre class="tool-out-content"><code>{{ outputText }}</code></pre>
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
/* ── Collapsed row ── */
.tool-card {
  width: 100%;
  min-width: 0;
}

.tool-row {
  display: flex;
  align-items: center;
  gap: 7px;
  width: 100%;
  padding: 4px 8px;
  border: 0;
  border-radius: var(--r-sm);
  background: transparent;
  color: var(--text-muted);
  font: inherit;
  font-size: 13px;
  line-height: 1.5;
  text-align: left;
  cursor: pointer;
  user-select: text;
  transition: background 150ms ease, color 150ms ease;
}

.tool-row:hover,
.tool-row:focus-visible {
  background: color-mix(in srgb, var(--accent) 8%, transparent);
  color: var(--text);
}

.tool-chevron {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 9px;
  font-size: 12px;
  flex-shrink: 0;
  color: var(--text-muted);
  opacity: 0.7;
  transition: transform 150ms ease;
}

.tool-row[aria-expanded="true"] .tool-chevron {
  transform: rotate(90deg);
}

.tool-icon {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 15px;
  flex-shrink: 0;
  color: var(--text-muted);
}

.tool-prompt {
  flex-shrink: 0;
  font-family: ui-monospace, "Cascadia Code", monospace;
  font-weight: 600;
  color: var(--success);
}

.tool-cmd {
  flex: 0 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: ui-monospace, "Cascadia Code", monospace;
  font-size: 12px;
  color: var(--text);
}

.tool-verb {
  flex-shrink: 0;
  font-weight: 500;
  color: var(--text);
}

.tool-target {
  flex: 0 1 auto;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: ui-monospace, "Cascadia Code", monospace;
  font-size: 12px;
  font-weight: 500;
  color: var(--text);
}

.tool-detail {
  flex: 0 100000 auto;
  min-width: 24px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-family: ui-monospace, "Cascadia Code", monospace;
  font-size: 12px;
  color: var(--text-muted);
}

.tool-badge {
  flex-shrink: 0;
  margin-left: auto;
  padding: 1px 6px;
  border-radius: 10px;
  background: color-mix(in srgb, var(--danger) 14%, transparent);
  color: var(--danger);
  font-size: 11px;
  font-weight: 600;
}

.tool-spinner {
  flex-shrink: 0;
  margin-left: auto;
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--accent);
  animation: pulse 1.1s ease-in-out infinite;
}

@keyframes pulse {
  0%, 100% { opacity: 0.35; transform: scale(0.85); }
  50%      { opacity: 1;    transform: scale(1); }
}

.is-running .tool-icon {
  color: var(--accent);
}

/* ── Expanded body ── */
.tool-body {
  padding: 2px 8px 8px 24px;
}

.tool-term {
  margin-top: 6px;
  border-radius: var(--r-sm);
  background: color-mix(in srgb, var(--n-2) 55%, transparent);
  overflow: hidden;
}

.tool-term-cmd {
  display: flex;
  align-items: baseline;
  gap: 8px;
  padding: 8px 12px 0;
  font-family: ui-monospace, "Cascadia Code", monospace;
  font-size: 12px;
  color: var(--text);
}

.tool-term-cmd code {
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

.tool-term-out {
  margin: 0;
  padding: 6px 12px 10px;
  font-family: ui-monospace, "Cascadia Code", monospace;
  font-size: 12px;
  line-height: 1.5;
  color: var(--text-muted);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  overflow: auto;
  max-height: min(420px, 55vh);
}

.tool-term.is-error .tool-term-out {
  color: color-mix(in srgb, var(--danger) 70%, var(--text) 30%);
}

/* ── Key-value args ── */
.tool-kv {
  display: flex;
  flex-direction: column;
  gap: 2px;
  margin-top: 6px;
  font-size: 13px;
  line-height: 1.6;
}

.tool-kv-row {
  display: flex;
  align-items: baseline;
  gap: 8px;
  min-width: 0;
}

.tool-kv-key {
  flex-shrink: 0;
  font-family: ui-monospace, "Cascadia Code", monospace;
  font-size: 12px;
  color: var(--text-muted);
}

.tool-kv-value {
  min-width: 0;
  color: var(--text);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
}

/* ── Generic output block ── */
.tool-out {
  margin-top: 6px;
}

.tool-out-label {
  display: inline-block;
  margin-bottom: 4px;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.04em;
  text-transform: uppercase;
  color: var(--text-muted);
}

.tool-out-content {
  margin: 0;
  padding: 10px 12px;
  border-radius: var(--r-sm);
  background: color-mix(in srgb, var(--n-2) 55%, transparent);
  color: var(--text);
  font-size: 12px;
  line-height: 1.45;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  overflow: auto;
  max-height: min(520px, 60vh);
}

.tool-out.is-error .tool-out-content {
  color: color-mix(in srgb, var(--danger) 70%, var(--text) 30%);
}

@media (prefers-reduced-motion: reduce) {
  .tool-spinner { animation: none; opacity: 0.8; }
}
</style>
