<script setup>
import { computed } from 'vue'
import { parseCommandOutput } from '../lib/commandOutput.js'

/**
 * A slash command and its result, as one left-aligned card.
 *
 * Previously both halves rendered as centered 12px system notes, which threw
 * away the alignment the daemon puts into its output (`/help`'s two columns
 * wrapped into a centered paragraph). Here the echoed command is a header and
 * the output keeps its structure: field chips, a term/description grid, or a
 * verbatim monospace block.
 *
 * `msg` shape (see state.js appendCommandMessage):
 *   { id, role: 'command', command, content, error? }
 */
const props = defineProps({
  msg: { type: Object, required: true },
})

const parsed = computed(() => parseCommandOutput(props.msg.content))
const isError = computed(() => Boolean(props.msg.error))
</script>

<template>
  <div class="cmd-card" :class="{ 'is-error': isError }" role="note">
    <div class="cmd-head">
      <span class="cmd-slash" aria-hidden="true">/</span>
      <code class="cmd-name">{{ msg.command.replace(/^\//, '') }}</code>
    </div>

    <!-- k:v groups → label/value chips (/status, /model) -->
    <div v-if="parsed.kind === 'fields'" class="cmd-fields">
      <div v-for="(row, i) in parsed.rows" :key="i" class="cmd-field-row">
        <span v-for="f in row" :key="f.key" class="cmd-field">
          <span class="cmd-field-key">{{ f.key }}</span>
          <span class="cmd-field-value">{{ f.value || '—' }}</span>
        </span>
      </div>
    </div>

    <!-- term + description → aligned grid (/help) -->
    <dl v-else-if="parsed.kind === 'pairs'" class="cmd-pairs">
      <template v-for="row in parsed.rows" :key="row.term">
        <dt><code>{{ row.term }}</code></dt>
        <dd>{{ row.desc }}</dd>
      </template>
    </dl>

    <p v-else-if="parsed.kind === 'note'" class="cmd-note">{{ parsed.text }}</p>

    <pre v-else-if="parsed.kind === 'text'" class="cmd-text">{{ parsed.text }}</pre>
  </div>
</template>

<style scoped>
.cmd-card {
  align-self: stretch;
  min-width: 0;
  max-width: min(840px, 100%);
  padding: var(--sp-2) var(--sp-3);
  border: 1px solid var(--border);
  border-left: 2px solid var(--accent-muted);
  border-radius: var(--r-md);
  background: color-mix(in srgb, var(--n-2) 22%, transparent);
  font-size: 13px;
}

.cmd-card.is-error {
  border-left-color: var(--danger);
}

.cmd-head {
  display: flex;
  align-items: baseline;
  gap: 1px;
  font-family: ui-monospace, "Cascadia Code", "Fira Code", monospace;
  font-size: 12px;
}

.cmd-slash {
  color: var(--accent);
  font-weight: 600;
}

.cmd-name {
  font: inherit;
  font-weight: 600;
  color: var(--text);
  overflow-wrap: anywhere;
}

/* ── k:v fields ── */
.cmd-fields {
  display: flex;
  flex-direction: column;
  gap: var(--sp-1);
  margin-top: var(--sp-2);
}

.cmd-field-row {
  display: flex;
  flex-wrap: wrap;
  gap: var(--sp-1) var(--sp-4);
}

.cmd-field {
  display: inline-flex;
  align-items: baseline;
  gap: 6px;
  min-width: 0;
}

.cmd-field-key {
  color: var(--text-muted);
  font-size: 12px;
  white-space: nowrap;
}

.cmd-field-value {
  font-family: ui-monospace, "Cascadia Code", "Fira Code", monospace;
  font-size: 12px;
  color: var(--text);
  overflow-wrap: anywhere;
}

/* ── term/description grid ──
   The term column sizes to its widest entry so descriptions share one left
   edge, which is what the daemon's space-padding was reaching for. */
.cmd-pairs {
  display: grid;
  grid-template-columns: max-content minmax(0, 1fr);
  gap: 2px var(--sp-4);
  margin: var(--sp-2) 0 0;
}

.cmd-pairs dt {
  font-family: ui-monospace, "Cascadia Code", "Fira Code", monospace;
  font-size: 12px;
}

.cmd-pairs dt code {
  font: inherit;
  color: var(--accent);
  white-space: nowrap;
}

.cmd-pairs dd {
  margin: 0;
  color: var(--text-muted);
  overflow-wrap: anywhere;
}

/* On narrow panes the two columns would squeeze the description to a few
   characters per line; stack instead. */
@media (max-width: 560px) {
  .cmd-pairs {
    grid-template-columns: minmax(0, 1fr);
    gap: 0;
  }
  .cmd-pairs dd {
    margin: 0 0 var(--sp-2);
  }
}

/* ── one-line prose ── */
.cmd-note {
  margin: var(--sp-1) 0 0;
  color: var(--text-muted);
  overflow-wrap: anywhere;
}

.cmd-card.is-error .cmd-note {
  color: color-mix(in srgb, var(--danger) 70%, var(--text) 30%);
}

/* ── verbatim block ── */
.cmd-text {
  margin: var(--sp-2) 0 0;
  font-family: ui-monospace, "Cascadia Code", "Fira Code", monospace;
  font-size: 12px;
  line-height: 1.55;
  color: var(--text);
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  overflow-x: auto;
  max-height: min(480px, 55vh);
}

.cmd-card.is-error .cmd-text {
  color: color-mix(in srgb, var(--danger) 70%, var(--text) 30%);
}
</style>
