/**
 * Markdown rendering for assistant messages.
 *
 * `marked` parses Markdown → HTML, `DOMPurify` sanitizes the result before it is
 * inserted via `v-html`. We never trust model output: any raw HTML in the
 * stream is neutralized (scripts, event handlers, external embeds, etc.).
 *
 * Streaming note: `pending` messages render as they stream; callers pass the
 * same content they already have. Markdown that is mid-token (e.g. a half-open
 * code fence or bold) may briefly render literally and settle once the token
 * completes — an accepted tradeoff over re-parsing the whole buffer on every
 * delta.
 */

import { marked } from 'marked'
import DOMPurify from 'dompurify'

// Configure once at module load. gfm: tables/strikethrough/tasklists/autolink;
// breaks: single newlines become <br> (matches chat expectations for prose).
marked.setOptions({
  gfm: true,
  breaks: true,
})

/**
 * Render a markdown string to sanitized HTML.
 *
 * @param {string} src raw markdown (may be empty while streaming)
 * @returns {string} sanitized HTML, or '' for empty input
 */
export function renderMarkdown(src) {
  if (!src || !src.trim()) return ''
  const raw = marked.parse(src, { async: false })
  return DOMPurify.sanitize(raw)
}
