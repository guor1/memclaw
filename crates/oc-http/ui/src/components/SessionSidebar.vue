<script setup>
import { sessions, activeSessionId, loadSessions } from '../lib/state.js'

defineProps({
  onSelect: { type: Function, default: null },
})

const emit = defineEmits(['select'])

function select(id) {
  emit('select', id)
}

// Format unix timestamp to a short time/date string.
function formatDate(ts) {
  const d = new Date(ts * 1000)
  const now = new Date()
  const today = now.toDateString() === d.toDateString()
  return today
    ? d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
    : d.toLocaleDateString([], { month: 'short', day: 'numeric' })
}
</script>

<template>
  <nav class="sidebar" aria-label="会话列表">
    <header class="sidebar-header">
      <span class="logo">🦞</span>
      <span class="title">oh-my-claw</span>
    </header>

    <div class="session-list" role="list">
      <template v-if="sessions.loading">
        <div
          v-for="i in 4"
          :key="`skeleton-${i}`"
          class="skeleton"
          role="presentation"
          aria-hidden="true"
        ></div>
      </template>
      <div v-else-if="sessions.error" class="list-error">
        <span>加载失败</span>
        <button class="retry-btn" @click="loadSessions()">重试</button>
      </div>
      <div v-else-if="sessions.list.length === 0" class="empty">暂无会话</div>
      <template v-else>
        <button
          v-for="session in sessions.list"
          :key="session.id"
          class="session-item"
          :class="{ active: session.id === activeSessionId.value }"
          :aria-current="session.id === activeSessionId.value ? 'page' : undefined"
          @click="select(session.id)"
        >
          <span class="session-id">{{ session.id }}</span>
          <span class="session-meta">{{ formatDate(session.created_at) }}</span>
        </button>
      </template>
    </div>
  </nav>
</template>

<style scoped>
.sidebar {
  display: flex;
  flex-direction: column;
  background: var(--surface-raised);
  border-right: 1px solid var(--border);
  overflow: hidden;
}

.sidebar-header {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-4);
  border-bottom: 1px solid var(--border);
  font-weight: 600;
  font-size: 15px;
  flex-shrink: 0;
}

.logo { font-size: 20px; line-height: 1; }

.session-list {
  flex: 1;
  overflow-y: auto;
  padding: var(--sp-2) 0;
}

.session-item {
  display: flex;
  flex-direction: column;
  width: 100%;
  padding: var(--sp-2) var(--sp-4);
  text-align: left;
  gap: 2px;
  border-radius: 0;
  transition: background 120ms ease;
}

.session-item:hover { background: color-mix(in srgb, var(--accent) 8%, transparent); }

.session-item.active {
  background: var(--accent-bg);
  color: var(--accent);
}

.session-id {
  font-size: 13px;
  font-weight: 500;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

.session-meta {
  font-size: 11px;
  color: var(--text-muted);
}

.active .session-meta { color: var(--accent-muted); }

.skeleton {
  height: 40px;
  margin: var(--sp-1) var(--sp-4);
  border-radius: var(--r-sm);
  background: linear-gradient(
    90deg,
    var(--border) 25%,
    color-mix(in srgb, var(--border) 60%, transparent) 50%,
    var(--border) 75%
  );
  background-size: 200% 100%;
  animation: shimmer 1.4s ease infinite;
}

@keyframes shimmer {
  from { background-position: 200% 0; }
  to   { background-position: -200% 0; }
}

.list-error {
  display: flex;
  align-items: center;
  gap: var(--sp-2);
  padding: var(--sp-4);
  color: var(--danger);
  font-size: 13px;
}

.retry-btn {
  font-size: 12px;
  color: var(--accent);
  text-decoration: underline;
  padding: 0;
}

.empty {
  padding: var(--sp-4);
  color: var(--text-muted);
  font-size: 13px;
}
</style>
