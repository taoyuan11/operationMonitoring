<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { FitAddon } from '@xterm/addon-fit'
import { Terminal } from '@xterm/xterm'
import '@xterm/xterm/css/xterm.css'
import { terminalWebSocketUrl } from '../api/terminal'
import type { TerminalSessionStatus } from '../types/domain'

const props = defineProps<{
  instanceId: string
  shellProgram: string | null
  active: boolean
}>()

const emit = defineEmits<{
  status: [status: TerminalSessionStatus]
}>()

const terminalElement = ref<HTMLDivElement | null>(null)
let socket: WebSocket | null = null
let terminal: Terminal | null = null
let fitAddon: FitAddon | null = null
let resizeObserver: ResizeObserver | null = null
let closedByUser = false
let currentStatus: TerminalSessionStatus = 'opening'

onMounted(() => {
  terminal = new Terminal({
    cursorBlink: true,
    cursorStyle: 'block',
    convertEol: true,
    fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace',
    fontSize: 14,
    scrollback: 10_000,
    theme: {
      background: '#0d141c',
      foreground: '#e6edf3',
      cursor: '#6cb6ff',
      selectionBackground: '#264f78',
    },
  })
  fitAddon = new FitAddon()
  terminal.loadAddon(fitAddon)
  terminal.open(terminalElement.value!)
  terminal.onData((data) => sendMessage({ type: 'input', data: encodeUtf8(data) }))
  terminal.onResize(({ cols, rows }) => sendMessage({ type: 'resize', cols, rows }))

  socket = new WebSocket(terminalWebSocketUrl(props.instanceId, props.shellProgram))
  socket.onopen = () => {
    setStatus('opening')
    fitTerminal()
  }
  socket.onmessage = (event) => {
    if (typeof event.data !== 'string') return
    try {
      const message = JSON.parse(event.data) as TerminalServerMessage
      switch (message.type) {
        case 'opening':
          setStatus('opening')
          break
        case 'ready':
          setStatus('ready')
          fitTerminal()
          terminal?.focus()
          break
        case 'output':
          terminal?.write(decodeBase64(message.data))
          break
        case 'closed':
          setStatus('closed')
          terminal?.writeln(
            `\r\n\x1b[33m[终端已关闭${message.exit_code == null ? '' : `，退出码 ${message.exit_code}`}${message.reason ? `：${message.reason}` : ''}]\x1b[0m`,
          )
          break
        case 'error':
          setStatus('error')
          terminal?.writeln(`\r\n\x1b[31m[${message.message}]\x1b[0m`)
          break
      }
    } catch {
      setStatus('error')
      terminal?.writeln('\r\n\x1b[31m[收到无法解析的终端消息]\x1b[0m')
    }
  }
  socket.onerror = () => {
    if (currentStatus === 'closed' || currentStatus === 'error') return
    setStatus('error')
    terminal?.writeln('\r\n\x1b[31m[终端连接发生错误]\x1b[0m')
  }
  socket.onclose = () => {
    if (closedByUser || currentStatus === 'closed' || currentStatus === 'error') return
    setStatus('disconnected')
    terminal?.writeln('\r\n\x1b[33m[终端连接已断开]\x1b[0m')
  }

  resizeObserver = new ResizeObserver(fitTerminal)
  resizeObserver.observe(terminalElement.value!)
  window.addEventListener('resize', fitTerminal)
  requestAnimationFrame(fitTerminal)
})

watch(
  () => props.active,
  async (active) => {
    if (!active) return
    await nextTick()
    requestAnimationFrame(() => {
      fitTerminal()
      terminal?.focus()
    })
  },
)

onBeforeUnmount(() => {
  closedByUser = true
  resizeObserver?.disconnect()
  window.removeEventListener('resize', fitTerminal)
  socket?.close()
  terminal?.dispose()
  socket = null
  terminal = null
  fitAddon = null
})

function setStatus(status: TerminalSessionStatus) {
  currentStatus = status
  emit('status', status)
}

function fitTerminal() {
  if (!props.active || !terminal || !fitAddon || !terminalElement.value) return
  try {
    fitAddon.fit()
  } catch {
    // A tab may be switching while the observer runs.
  }
}

function sendMessage(message: TerminalClientMessage) {
  if (socket?.readyState === WebSocket.OPEN) socket.send(JSON.stringify(message))
}

function encodeUtf8(value: string) {
  const bytes = new TextEncoder().encode(value)
  let binary = ''
  for (let index = 0; index < bytes.length; index += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(index, index + 0x8000))
  }
  return btoa(binary)
}

function decodeBase64(value: string) {
  const binary = atob(value)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index)
  return bytes
}

type TerminalClientMessage =
  | { type: 'input'; data: string }
  | { type: 'resize'; cols: number; rows: number }

type TerminalServerMessage =
  | { type: 'opening' }
  | { type: 'ready' }
  | { type: 'output'; data: string }
  | { type: 'closed'; exit_code: number | null; reason: string | null }
  | { type: 'error'; message: string }
</script>

<template>
  <div class="terminal-session-pane">
    <div class="terminal-screen" aria-label="交互式终端">
      <div ref="terminalElement" class="terminal-screen-content"></div>
    </div>
  </div>
</template>
