<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { Download, Pause, Play, ScrollText, Trash2, X } from '@lucide/vue'
import { dockerWebSocketUrl } from '../api/docker'
import type { DockerLogServerMessage } from '../types/docker'

const MAX_LINES = 10_000
const MAX_BYTES = 5 * 1024 * 1024
const MAX_RECONNECT_ATTEMPTS = 7
const RECONNECT_BASE_DELAY_MS = 500
const RECONNECT_MAX_DELAY_MS = 15_000
const RECONNECT_STABLE_MS = 30_000
const textEncoder = new TextEncoder()

const props = defineProps<{
  instanceId: string
  containerId: string
  containerName: string
}>()

const emit = defineEmits<{ close: [] }>()

const tail = ref(200)
const following = ref(true)
const status = ref('正在连接')
const lines = ref<string[]>([])
const partialLine = ref('')
const logElement = ref<HTMLElement | null>(null)
let socket: WebSocket | null = null
let cursor: string | number | null = null
let bufferedBytes = 0
let disposed = false
let reconnectAttempts = 0
let reconnectTimer: number | null = null
let stableConnectionTimer: number | null = null
let resumeBoundary: { cursor: string; remainingLines: Map<string, number> } | null = null

watch(tail, () => {
  lines.value = []
  bufferedBytes = 0
  partialLine.value = ''
  cursor = null
  resumeBoundary = null
  reconnect()
})

onMounted(connect)
onBeforeUnmount(() => {
  disposed = true
  clearReconnectTimer()
  clearStableConnectionTimer()
  const previous = socket
  socket = null
  previous?.close(1000, 'client_closed')
})

function connect() {
  if (disposed || !following.value) return
  clearReconnectTimer()
  clearStableConnectionTimer()
  status.value = '正在连接'
  prepareResumeBoundary()
  const nextSocket = new WebSocket(dockerWebSocketUrl(
    props.instanceId,
    `containers/${encodeURIComponent(props.containerId)}/logs/ws`,
    {
      tail: Math.max(200, Math.min(2000, tail.value)),
      follow: true,
      since: cursor,
      timestamps: true,
    },
  ))
  socket = nextSocket
  nextSocket.onopen = () => {
    if (socket === nextSocket) status.value = '持续跟随'
  }
  nextSocket.onmessage = (event) => {
    if (socket === nextSocket) handleMessage(event.data)
  }
  nextSocket.onerror = () => {
    if (socket === nextSocket) status.value = '连接错误'
  }
  nextSocket.onclose = (event) => {
    if (socket !== nextSocket) return
    socket = null
    clearStableConnectionTimer()
    if (disposed || !following.value) return
    if (isAuthenticationClose(event)) {
      following.value = false
      status.value = closeStatus(event)
      return
    }
    scheduleReconnect()
  }
}

function reconnect() {
  clearReconnectTimer()
  clearStableConnectionTimer()
  reconnectAttempts = 0
  const previous = socket
  socket = null
  previous?.close(1000, 'reconnecting')
  if (following.value) connect()
}

function toggleFollowing() {
  following.value = !following.value
  if (following.value) {
    reconnectAttempts = 0
    connect()
  } else {
    status.value = '已暂停'
    clearReconnectTimer()
    clearStableConnectionTimer()
    const previous = socket
    socket = null
    previous?.close(1000, 'paused')
  }
}

function handleMessage(raw: unknown) {
  if (typeof raw !== 'string') return
  try {
    const message = JSON.parse(raw) as DockerLogServerMessage
    if (message.type === 'ready' || message.type === 'opening') {
      status.value = message.type === 'ready' ? '持续跟随' : '正在连接'
      if (message.type === 'ready') scheduleStableReconnectReset()
      if (message.cursor != null) cursor = message.cursor
      return
    }
    if (message.type === 'output' || message.type === 'line' || message.type === 'chunk') {
      cursor = message.cursor ?? message.ts ?? cursor
      const text = message.encoding === 'base64' ? decodeBase64(message.data) : message.data
      append(text)
      return
    }
    if (message.type === 'closed') {
      if (message.cursor != null) cursor = message.cursor
      status.value = message.reason || (message.retryable ? '日志流已断开' : '流已结束')
      if (message.retryable === true) return
      following.value = false
      clearReconnectTimer()
      clearStableConnectionTimer()
      return
    }
    if (message.type === 'error') {
      status.value = message.message
      if (message.retryable !== true) {
        following.value = false
        clearReconnectTimer()
        clearStableConnectionTimer()
        return
      }
    }
  } catch {
    append(raw)
  }
}

function scheduleReconnect() {
  if (disposed || !following.value || reconnectTimer !== null) return
  if (reconnectAttempts >= MAX_RECONNECT_ATTEMPTS) {
    following.value = false
    status.value = '多次重连失败，已停止跟随'
    return
  }

  const delay = Math.min(
    RECONNECT_BASE_DELAY_MS * 2 ** reconnectAttempts,
    RECONNECT_MAX_DELAY_MS,
  )
  reconnectAttempts += 1
  status.value = `连接已断开，${formatReconnectDelay(delay)}后重连`
  reconnectTimer = window.setTimeout(() => {
    reconnectTimer = null
    connect()
  }, delay)
}

function clearReconnectTimer() {
  if (reconnectTimer === null) return
  window.clearTimeout(reconnectTimer)
  reconnectTimer = null
}

function scheduleStableReconnectReset() {
  clearStableConnectionTimer()
  const activeSocket = socket
  stableConnectionTimer = window.setTimeout(() => {
    stableConnectionTimer = null
    if (socket === activeSocket && activeSocket?.readyState === WebSocket.OPEN) {
      reconnectAttempts = 0
    }
  }, RECONNECT_STABLE_MS)
}

function clearStableConnectionTimer() {
  if (stableConnectionTimer === null) return
  window.clearTimeout(stableConnectionTimer)
  stableConnectionTimer = null
}

function isAuthenticationClose(event: CloseEvent) {
  return event.code === 1008 || event.code === 4401 || event.code === 4403
}

function closeStatus(event: CloseEvent) {
  if (event.code === 1008 || event.code === 4401 || event.code === 4403) {
    return '鉴权失败，日志跟随已停止'
  }
  return event.reason || '日志流已正常关闭'
}

function formatReconnectDelay(delay: number) {
  return delay < 1000 ? `${delay} 毫秒` : `${delay / 1000} 秒`
}

function append(chunk: string) {
  const joined = partialLine.value + chunk
  const chunks = joined.split(/\r?\n/)
  partialLine.value = chunks.pop() || ''
  const nextLines = chunks.filter((line) => !isResumeBoundaryDuplicate(line))
  if (nextLines.length) {
    lines.value.push(...nextLines)
    for (const line of nextLines) bufferedBytes += textEncoder.encode(line).byteLength + 1
  }
  trimBuffer()
  void nextTick(() => {
    if (following.value && logElement.value) logElement.value.scrollTop = logElement.value.scrollHeight
  })
}

function trimBuffer() {
  const partialBytes = textEncoder.encode(partialLine.value).byteLength
  while (
    lines.value.length + (partialLine.value ? 1 : 0) > MAX_LINES
    || bufferedBytes + partialBytes > MAX_BYTES
  ) {
    const removed = lines.value.shift()
    if (removed === undefined) break
    bufferedBytes -= textEncoder.encode(removed).byteLength + 1
  }
  if (partialBytes > MAX_BYTES) {
    const encoded = textEncoder.encode(partialLine.value)
    partialLine.value = new TextDecoder().decode(encoded.slice(encoded.length - MAX_BYTES))
  }
}

function prepareResumeBoundary() {
  if (typeof cursor !== 'string' || !cursor) {
    resumeBoundary = null
    return
  }
  const remainingLines = new Map<string, number>()
  for (const line of lines.value) {
    if (!hasCursor(line, cursor)) continue
    remainingLines.set(line, (remainingLines.get(line) || 0) + 1)
  }
  resumeBoundary = { cursor, remainingLines }
  // A reconnect replays the cursor boundary, so replace any unfinished copy with that full line.
  partialLine.value = ''
}

function isResumeBoundaryDuplicate(line: string) {
  const boundary = resumeBoundary
  if (!boundary) return false
  if (hasCursor(line, boundary.cursor)) {
    const remaining = boundary.remainingLines.get(line) || 0
    if (!remaining) return false
    if (remaining === 1) boundary.remainingLines.delete(line)
    else boundary.remainingLines.set(line, remaining - 1)
    if (!boundary.remainingLines.size) resumeBoundary = null
    return true
  }
  const lineCursor = cursorPrefix(line)
  if (lineCursor && lineCursor > boundary.cursor) resumeBoundary = null
  return false
}

function hasCursor(line: string, value: string) {
  return line.startsWith(`${value} `) || line.startsWith(`${value}\t`)
}

function cursorPrefix(line: string) {
  const separator = line.search(/[ \t]/)
  if (separator <= 0) return null
  const candidate = line.slice(0, separator)
  return candidate.includes('T') ? candidate : null
}

function clearLogs() {
  lines.value = []
  bufferedBytes = 0
  partialLine.value = ''
}

function downloadLogs() {
  const blob = new Blob([displayText()], { type: 'text/plain;charset=utf-8' })
  const link = document.createElement('a')
  link.href = URL.createObjectURL(blob)
  link.download = `${props.containerName || props.containerId}-logs.txt`
  link.click()
  URL.revokeObjectURL(link.href)
}

function displayText() {
  const complete = lines.value.join('\n')
  if (!partialLine.value) return complete
  return complete ? `${complete}\n${partialLine.value}` : partialLine.value
}

function decodeBase64(value: string) {
  try {
    const binary = atob(value)
    const bytes = Uint8Array.from(binary, (character) => character.charCodeAt(0))
    return new TextDecoder().decode(bytes)
  } catch {
    return value
  }
}
</script>

<template>
  <div class="docker-overlay">
    <section class="docker-stream-window" role="dialog" aria-modal="true" aria-label="容器日志">
      <header class="docker-stream-head">
        <div class="docker-stream-title">
          <ScrollText :size="17" />
          <div><strong>{{ containerName }}</strong><span>{{ status }} · {{ (lines.length + (partialLine ? 1 : 0)).toLocaleString() }} 行</span></div>
        </div>
        <div class="docker-stream-actions">
          <label class="docker-tail-select">
            <span>尾部行数</span>
            <select v-model.number="tail">
              <option :value="200">200</option>
              <option :value="500">500</option>
              <option :value="1000">1000</option>
              <option :value="2000">2000</option>
            </select>
          </label>
          <button class="icon-button subtle" type="button" :title="following ? '暂停日志' : '继续日志'" @click="toggleFollowing">
            <Pause v-if="following" :size="15" /><Play v-else :size="15" />
          </button>
          <button class="icon-button subtle" type="button" title="下载日志" :disabled="!lines.length && !partialLine" @click="downloadLogs">
            <Download :size="15" />
          </button>
          <button class="icon-button subtle" type="button" title="清空显示" :disabled="!lines.length && !partialLine" @click="clearLogs">
            <Trash2 :size="15" />
          </button>
          <button class="icon-button subtle" type="button" title="关闭日志" @click="emit('close')">
            <X :size="16" />
          </button>
        </div>
      </header>
      <pre ref="logElement" class="docker-log-output" aria-live="polite">{{ displayText() || '等待日志输出…' }}</pre>
    </section>
  </div>
</template>
