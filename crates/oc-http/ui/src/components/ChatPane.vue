<script setup>
import { ref, computed, watchEffect } from 'vue'
import MessageBubble from './MessageBubble.vue'
import ApprovalModal from './ApprovalModal.vue'
import { sendChat, sendCommand, approvalReply, userReply } from '../lib/api.js'
import {
  messagesFor,
  appendMessage,
  appendSystemMessage,
  clearSessionMessages,
  updateLastAssistant,
  finalizeLastAssistant,
  activeChats,
  setActiveChatCtrl,
  clearActiveChatCtrl,
  loadSessions,
  loadHistory,
  activeSessionId,
} from '../lib/state.js'

const props = defineProps({
  sessionId: { type: String, required: true },
})

const draft = ref('')
const scroller = ref(null)
const pendingApproval = ref(null)   // { approval_id, summary, command }
const pendingInput = ref(null)      // { input_id, prompt }
const errorText = ref(null)

const messages = computed(() => messagesFor(props.sessionId))
const isStreaming = computed(() => activeChats.has(props.sessionId))

// Autoscroll when message count or the last message's length changes.
watchEffect(() => {
  const len = messages.value.length
  const tail = messages.value.at(-1)?.content?.length
  if (scroller.value) {
    // rAF so the DOM has painted the new content before we measure.
    requestAnimationFrame(() => {
      scroller.value.scrollTop = scroller.value.scrollHeight
    })
  }
})

function submit() {
  const text = draft.value.trim()
  if (!text || isStreaming.value) return

  // Slash command: forward to the daemon (sole parser), never to the model.
  if (text.startsWith('/')) {
    submitCommand(text)
    return
  }

  errorText.value = null
  appendMessage(props.sessionId, {
    id: `user-${Date.now()}`,
    role: 'user',
    content: text,
  })
  draft.value = ''

  // Capture sessionId so callbacks write to the right session even if the
  // user switches away mid-stream (back-keep semantics).
  const target = props.sessionId

  const ctrl = sendChat({
    session: target,
    text,
    onAccepted() {
      // Refresh the list so a brand-new session shows up in the sidebar.
      // silent: true keeps the existing list visible and skips the skeleton flash.
      loadSessions({ silent: true })
    },
    onDelta(delta) {
      updateLastAssistant(target, delta)
    },
    onTool(ev) {
      if (ev.phase?.phase === 'start') {
        appendMessage(target, {
          id: `tool-${ev.call_id}`,
          role: 'tool',
          name: ev.phase.name,
          args: ev.phase.args ?? '',
          content: '',
          output: '',
          toolStatus: 'running',
          status: 'running',
        })
      } else if (ev.phase?.phase === 'update') {
        // Stream tool progress into the card's output buffer.
        const list = messagesFor(target)
        const m = list.find(x => x.id === `tool-${ev.call_id}`)
        if (m) {
          m.output = (m.output ?? '') + (ev.phase.chunk ?? '')
          m.content = m.output
        }
      } else if (ev.phase?.phase === 'end') {
        const list = messagesFor(target)
        const m = list.find(x => x.id === `tool-${ev.call_id}`)
        if (m) {
          m.toolStatus = ev.phase.status
          m.status = ev.phase.status
        }
      }
    },
    onApproval(ev) {
      pendingApproval.value = ev
    },
    onUserInput(ev) {
      pendingInput.value = ev
    },
    onEnd() {
      finalizeLastAssistant(target)
      clearActiveChatCtrl(target)
    },
    onError(msg) {
      finalizeLastAssistant(target)
      clearActiveChatCtrl(target)
      errorText.value = msg
    },
  })

  setActiveChatCtrl(target, ctrl)
}

async function submitCommand(text) {
  const target = props.sessionId
  appendSystemMessage(target, text)
  draft.value = ''
  try {
    const result = await sendCommand(target, text)
    if (result.clear_view && result.clear_view === target) {
      clearSessionMessages(target)
    }
    if (result.switch_session && result.switch_session !== target) {
      // Switch the active session: load its history and refresh the sidebar.
      activeSessionId.value = result.switch_session
      await loadHistory(result.switch_session)
      await loadSessions({ silent: true })
    }
    if (result.text) appendSystemMessage(result.switch_session ?? target, result.text)
  } catch (e) {
    errorText.value = e.message
  }
}

function stop() {
  activeChats.get(props.sessionId)?.abort()
  clearActiveChatCtrl(props.sessionId)
  finalizeLastAssistant(props.sessionId)
}

function onKeydown(e) {
  // Enter sends; Shift+Enter inserts a newline.
  if (e.key === 'Enter' && !e.shiftKey) {
    e.preventDefault()
    submit()
  }
}

async function respondApproval(allow) {
  const id = pendingApproval.value.approval_id
  pendingApproval.value = null
  try {
    await approvalReply(id, allow)
  } catch (e) {
    errorText.value = e.message
  }
}

async function respondInput(text) {
  const id = pendingInput.value.input_id
  pendingInput.value = null
  try {
    await userReply(id, text)
  } catch (e) {
    errorText.value = e.message
  }
}
</script>

<template>
  <div class="chat">
    <div ref="scroller" class="messages" role="log" aria-live="polite" aria-label="对话记录">
      <div v-if="messages.length === 0" class="empty-state">
        <p class="empty-title">开始对话</p>
        <p class="empty-hint">Enter 发送 · Shift+Enter 换行</p>
      </div>
      <template v-else>
        <MessageBubble v-for="msg in messages" :key="msg.id" :msg="msg" />
      </template>
    </div>

    <div v-if="errorText" class="error-banner" role="alert">
      <span>{{ errorText }}</span>
      <button @click="errorText = null" aria-label="关闭错误提示">✕</button>
    </div>

    <div class="composer">
      <textarea
        v-model="draft"
        @keydown="onKeydown"
        placeholder="说点什么…"
        rows="1"
        aria-label="消息输入框"
      ></textarea>
      <button v-if="isStreaming" class="btn stop" @click="stop">停止</button>
      <button v-else class="btn send" @click="submit" :disabled="!draft.trim()">发送</button>
    </div>
  </div>

  <ApprovalModal
    v-if="pendingApproval"
    kind="approval"
    :summary="pendingApproval.summary"
    :command="pendingApproval.command"
    @allow="respondApproval(true)"
    @deny="respondApproval(false)"
  />

  <ApprovalModal
    v-if="pendingInput"
    kind="input"
    :prompt="pendingInput.prompt"
    @submit="respondInput"
    @cancel="respondInput(null)"
  />
</template>

<style scoped>
.chat {
  display: grid;
  grid-template-rows: 1fr auto auto;
  overflow: hidden;
  min-height: 0;
}

.messages {
  overflow-y: auto;
  padding: var(--sp-5) var(--sp-5) var(--sp-4);
  display: flex;
  flex-direction: column;
  gap: var(--sp-4);
  scroll-behavior: smooth;
}

.empty-state {
  margin: auto;
  text-align: center;
  color: var(--text-muted);
}

.empty-title { font-size: 15px; margin-bottom: var(--sp-1); }
.empty-hint  { font-size: 12px; }

.error-banner {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: var(--sp-3);
  margin: 0 var(--sp-5) var(--sp-2);
  padding: var(--sp-2) var(--sp-3);
  background: color-mix(in srgb, var(--danger) 12%, transparent);
  border: 1px solid color-mix(in srgb, var(--danger) 40%, transparent);
  border-radius: var(--r-md);
  color: var(--danger);
  font-size: 13px;
}

.composer {
  display: flex;
  gap: var(--sp-2);
  align-items: flex-end;
  padding: var(--sp-3) var(--sp-5) var(--sp-4);
  border-top: 1px solid var(--border);
  background: var(--surface-raised);
}

textarea {
  flex: 1;
  resize: none;
  font: inherit;
  color: var(--text);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--r-md);
  padding: var(--sp-2) var(--sp-3);
  max-height: 30dvh;
  /* Grow with content up to max-height. */
  field-sizing: content;
}

textarea::placeholder { color: var(--text-muted); }

.btn {
  padding: var(--sp-2) var(--sp-4);
  border-radius: var(--r-md);
  font-size: 13px;
  font-weight: 500;
  transition: background 120ms ease, opacity 120ms ease;
  flex-shrink: 0;
}

.send {
  background: var(--accent);
  color: white;
}

.send:hover:not(:disabled) { background: var(--accent-hover); }

.send:disabled {
  opacity: 0.4;
  cursor: not-allowed;
}

.stop {
  background: var(--danger);
  color: white;
}

.stop:hover { filter: brightness(0.92); }
</style>
