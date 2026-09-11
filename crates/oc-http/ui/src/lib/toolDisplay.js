/**
 * Tool-name → display spec: kind classification + icon/verb/target resolution.
 *
 * A simplified port of openclaw's `tool-call-view.ts` (resolveToolCallKind) and
 * `tool-display.ts` (resolveToolDisplay), tuned to oh-my-claw's tool set:
 *   exec, file, sys, web_fetch, web_search, message, cron, ask_user, process.
 *
 * `resolveToolView(name, args)` returns a plain object consumed by ToolCard.vue.
 * No Vue/reactivity here — it's pure and cheap enough to call per render.
 */

const FILE_READ_OPS = new Set(['read', 'head', 'tail'])
const FILE_WRITE_OPS = new Set(['write', 'append'])
const FILE_EDIT_OPS = new Set(['edit'])
const FILE_SEARCH_OPS = new Set(['list', 'stat', 'grep', 'glob'])

/** Coerce the tool's args string/object into a plain object, or null. */
function asRecord(args) {
  if (!args) return null
  if (typeof args === 'object' && !Array.isArray(args)) return args
  if (typeof args === 'string') {
    const trimmed = args.trim()
    if (!trimmed) return null
    try {
      const v = JSON.parse(trimmed)
      return v && typeof v === 'object' && !Array.isArray(v) ? v : null
    } catch {
      return null
    }
  }
  return null
}

/** Split "C:\Users\x\foo.rs" → { base: "foo.rs", dir: "C:\Users\x" }. */
function splitPath(path) {
  const normalized = String(path).replace(/\\/g, '/').replace(/\/+$/, '')
  const slash = normalized.lastIndexOf('/')
  if (slash <= 0) return { base: normalized || String(path) }
  return { base: normalized.slice(slash + 1), dir: normalized.slice(0, slash) }
}

/** First non-empty line of a string, trimmed. */
function firstLine(s) {
  if (typeof s !== 'string') return undefined
  const line = s.split(/\r\n?|\n/).map((l) => l.trim()).find((l) => l.length > 0)
  return line || undefined
}

/**
 * Resolve a tool call to a display view.
 *
 * @param {string} name tool name ("exec", "file", "web_fetch", …)
 * @param {*} args  args JSON text or already-parsed object
 * @returns {{ kind, icon, verb, target, targetDetail, argsRecord }}
 */
export function resolveToolView(name, args) {
  const record = asRecord(args)
  const key = String(name || '').trim().toLowerCase()

  if (key === 'exec') {
    const command = record?.command
    return {
      kind: 'command',
      icon: '⌘',
      verb: '执行命令',
      command: command ? String(command) : '',
      target: command ? firstLine(String(command)) : undefined,
      argsRecord: record,
    }
  }

  if (key === 'file') {
    const op = String(record?.op ?? '').toLowerCase()
    const path = record?.path
    const pathParts = path ? splitPath(path) : null
    if (FILE_READ_OPS.has(op)) {
      return { kind: 'read', icon: '📄', verb: '读取', target: pathParts?.base, targetDetail: pathParts?.dir, argsRecord: record }
    }
    if (FILE_WRITE_OPS.has(op)) {
      return { kind: 'write', icon: '✏️', verb: '写入', target: pathParts?.base, targetDetail: pathParts?.dir, argsRecord: record }
    }
    if (FILE_EDIT_OPS.has(op)) {
      return { kind: 'edit', icon: '✎', verb: '编辑', target: pathParts?.base, targetDetail: pathParts?.dir, argsRecord: record }
    }
    if (FILE_SEARCH_OPS.has(op)) {
      const pattern = record?.pattern
      return { kind: 'search', icon: '🔍', verb: '查找', target: pattern || pathParts?.base, targetDetail: pattern ? pathParts?.dir : undefined, argsRecord: record }
    }
    return { kind: 'generic', icon: '📁', verb: '文件', target: pathParts?.base, argsRecord: record }
  }

  if (key === 'sys') {
    const op = String(record?.op ?? '').toLowerCase()
    if (op === 'cd') return { kind: 'generic', icon: '📂', verb: '切换目录', target: record?.path, argsRecord: record }
    if (op === 'now') return { kind: 'generic', icon: '🕐', verb: '查看时间', argsRecord: record }
    if (op === 'pwd') return { kind: 'generic', icon: '📍', verb: '当前目录', argsRecord: record }
    return { kind: 'generic', icon: '🛠️', verb: '系统', argsRecord: record }
  }

  if (key === 'web_fetch') {
    const url = record?.url
    return { kind: 'fetch', icon: '🌐', verb: '抓取', target: url ? String(url) : undefined, argsRecord: record }
  }

  if (key === 'web_search') {
    const query = record?.query
    return { kind: 'search', icon: '🔎', verb: '搜索', target: query ? String(query) : undefined, argsRecord: record }
  }

  if (key === 'message') {
    return { kind: 'generic', icon: '💬', verb: '发送消息', target: firstLine(record?.text), argsRecord: record }
  }

  if (key === 'cron') {
    return { kind: 'generic', icon: '⏰', verb: '定时任务', target: record?.prompt, argsRecord: record }
  }

  if (key === 'ask_user') {
    return { kind: 'generic', icon: '❓', verb: '提问', target: firstLine(record?.prompt ?? record?.question), argsRecord: record }
  }

  if (key === 'process') {
    return { kind: 'generic', icon: '⚙️', verb: '后台进程', target: record?.command, argsRecord: record }
  }

  return { kind: 'generic', icon: '🧩', verb: name || '工具', argsRecord: record }
}

/**
 * Whether tool output text signals an error, for historical entries that don't
 * carry an explicit status. Mirrors openclaw's `isToolErrorOutput` heuristics,
 * minus JSON-object probing (our tool results are plain text or `工具错误:` prefixed).
 */
export function isErrorOutput(text) {
  if (!text) return false
  const trimmed = text.trim()
  if (!trimmed) return false
  if (/^工具错误[:：]/.test(trimmed)) return true
  if (/^exec 失败[:：]/.test(trimmed)) return true
  if (/\[退出码:\s*[1-9]/.test(trimmed)) return true
  return false
}
