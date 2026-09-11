<script setup>
import { notification } from '../lib/state.js'
</script>

<template>
  <div v-if="notification.text" class="toast" role="alert" aria-live="assertive">
    <span class="icon" aria-hidden="true">🔔</span>
    <span class="text">{{ notification.text }}</span>
    <button class="close" @click="notification.text = null" aria-label="关闭通知">✕</button>
  </div>
</template>

<style scoped>
.toast {
  position: fixed;
  bottom: var(--sp-5);
  right: var(--sp-5);
  display: flex;
  align-items: flex-start;
  gap: var(--sp-2);
  max-width: min(24rem, calc(100vw - 2 * var(--sp-5)));
  padding: var(--sp-3) var(--sp-4);
  background: var(--surface-raised);
  border: 1px solid var(--border);
  border-left: 3px solid var(--accent);
  border-radius: var(--r-md);
  box-shadow: 0 8px 24px color-mix(in srgb, var(--n-9) 18%, transparent);
  font-size: 13px;
  z-index: 90;
  animation: slide-in 200ms cubic-bezier(0.2, 0.8, 0.3, 1);
}

@keyframes slide-in {
  from { transform: translateX(12px); opacity: 0; }
}

@media (prefers-reduced-motion: reduce) {
  .toast { animation: none; }
}

.icon { flex-shrink: 0; }

.text {
  flex: 1;
  white-space: pre-wrap;
  word-break: break-word;
}

.close {
  flex-shrink: 0;
  color: var(--text-muted);
  font-size: 12px;
  line-height: 1;
  padding: 2px;
}

.close:hover { color: var(--text); }
</style>
