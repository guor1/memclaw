<script setup>
import { onMounted } from 'vue'
import SessionSidebar from './components/SessionSidebar.vue'
import ChatPane from './components/ChatPane.vue'
import StatusBar from './components/StatusBar.vue'
import Notification from './components/Notification.vue'
import {
  loadSessions,
  loadHistory,
  startAmbientStream,
  activeSessionId,
} from './lib/state.js'

onMounted(async () => {
  startAmbientStream()
  await loadSessions()
  // Bootstrap history for the default session immediately.
  await loadHistory(activeSessionId.value)
})

async function handleSelectSession(id) {
  activeSessionId.value = id
  await loadHistory(id)
}
</script>

<template>
  <div class="app">
    <SessionSidebar @select="handleSelectSession" />
    <main class="main">
      <ChatPane :session-id="activeSessionId.value" />
      <StatusBar />
    </main>
  </div>

  <Notification />
</template>

<style>
:root {
  /* Neutral ramp (OKLCH-inspired lightness steps, encoded as HSL for browser compat) */
  --n-0:  hsl(220 15% 97%);   /* off-white surface */
  --n-1:  hsl(220 12% 93%);
  --n-2:  hsl(220 10% 86%);
  --n-3:  hsl(220  9% 74%);
  --n-4:  hsl(220  8% 58%);   /* muted text */
  --n-5:  hsl(220  7% 42%);
  --n-6:  hsl(220  8% 28%);
  --n-7:  hsl(220  9% 20%);   /* raised surface dark */
  --n-8:  hsl(220 10% 14%);   /* surface dark */
  --n-9:  hsl(220 12% 10%);   /* off-black */

  /* Accent (blue) */
  --accent-h: 218;
  --accent:       hsl(var(--accent-h) 80% 52%);
  --accent-hover: hsl(var(--accent-h) 80% 44%);
  --accent-muted: hsl(var(--accent-h) 60% 70%);
  --accent-bg:    hsl(var(--accent-h) 80% 96%);

  /* Semantic */
  --success: hsl(150 60% 38%);
  --danger:  hsl(  2 70% 50%);
  --warn:    hsl( 38 90% 46%);

  /* Roles — light mode */
  --surface:      var(--n-0);
  --surface-raised: white;
  --border:       var(--n-2);
  --text:         hsl(220 12% 13%);  /* ~13% lightness, meets AA */
  --text-muted:   var(--n-4);
  --sidebar-w:    220px;

  /* Radii */
  --r-sm: 4px;
  --r-md: 8px;

  /* Space rhythm (8px base) */
  --sp-1: 4px;
  --sp-2: 8px;
  --sp-3: 12px;
  --sp-4: 16px;
  --sp-5: 24px;
  --sp-6: 32px;
}

@media (prefers-color-scheme: dark) {
  :root {
    --surface:       var(--n-8);
    --surface-raised: var(--n-7);
    --border:        var(--n-6);
    --text:          var(--n-0);
    --text-muted:    var(--n-3);
    --accent:        hsl(var(--accent-h) 65% 62%);  /* lower chroma, higher L */
    --accent-hover:  hsl(var(--accent-h) 65% 70%);
    --accent-muted:  hsl(var(--accent-h) 45% 50%);
    --accent-bg:     hsl(var(--accent-h) 30% 18%);
  }
}

*,
*::before,
*::after {
  box-sizing: border-box;
  margin: 0;
  padding: 0;
}

body {
  font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
  font-size: 14px;
  line-height: 1.6;
  background: var(--surface);
  color: var(--text);
  height: 100dvh;
  overflow: hidden;
}

button {
  font: inherit;
  cursor: pointer;
  border: none;
  background: none;
  color: inherit;
}

button:focus-visible,
input:focus-visible,
textarea:focus-visible {
  outline: 2px solid var(--accent);
  outline-offset: 2px;
}

.app {
  display: grid;
  grid-template-columns: var(--sidebar-w) 1fr;
  height: 100dvh;
}

.main {
  display: grid;
  grid-template-rows: 1fr auto;
  overflow: hidden;
  border-left: 1px solid var(--border);
}
</style>
