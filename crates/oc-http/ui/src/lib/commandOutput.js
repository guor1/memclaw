/**
 * Shape a slash command's plain-text output for display.
 *
 * The daemon formats command output for humans, not machines (see
 * `oc-server/src/command.rs`), so this is a display-only heuristic over the two
 * shapes it actually emits. Anything unrecognized falls through to a verbatim
 * monospace block — plainer, but never wrong.
 *
 *   'fields'  `k:v` groups separated by 2+ spaces (`/status`, `/model`)
 *   'pairs'   a `term  description` table (`/help`)
 *   'note'    a single line of prose (`/clear`, `/stop`, rejections)
 *   'text'    everything else, verbatim
 *   'empty'   no output at all
 */

// Columns in the daemon's output are separated by two or more spaces; a single
// space is inside a value (`/memory search <q>`, `模型:doubao-seed 2.1`).
const GAP = /\s{2,}/
const COLON = /[:：]/

export function parseCommandOutput(raw) {
  const text = String(raw ?? '').replace(/\s+$/, '')
  if (text === '') return { kind: 'empty' }

  const lines = text.split('\n').filter((l) => l.trim() !== '')
  const cols = lines.map((l) => l.trim().split(GAP))

  // Every column on every line is `key:value` → labelled fields.
  if (cols.every((segs) => segs.length >= 2 && segs.every((s) => COLON.test(s)))) {
    return { kind: 'fields', rows: cols.map((segs) => segs.map(toField)) }
  }

  // Every line is `/usage  description` → a command reference (`/help`).
  // Keyed on the leading slash rather than on "two columns" alone: `/sessions`
  // and `/tasks` are also two columns when their trailing field is empty, and
  // they should not flip between a definition grid and a table depending on
  // whether that field happens to be filled in.
  if (lines.length >= 2 && cols.every((segs) => segs.length === 2 && segs[0].startsWith('/'))) {
    return { kind: 'pairs', rows: cols.map(([term, desc]) => ({ term, desc })) }
  }

  // One line with no column structure is a sentence ("已清空当前会话上下文…",
  // a usage rejection) — prose, so it reads as prose rather than as terminal
  // output. Multi-line or column-bearing output keeps its monospace alignment.
  if (lines.length === 1 && cols[0].length === 1) {
    return { kind: 'note', text: lines[0].trim() }
  }

  return { kind: 'text', text }
}

function toField(segment) {
  const at = segment.search(COLON)
  return { key: segment.slice(0, at), value: segment.slice(at + 1).trim() }
}
