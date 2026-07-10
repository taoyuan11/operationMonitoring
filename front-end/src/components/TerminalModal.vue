<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { FitAddon } from '@xterm/addon-fit'
import { Terminal } from '@xterm/xterm'
import '@xterm/xterm/css/xterm.css'
import { X } from 'lucide-vue-next'
import type { Instance } from '../types/domain'

const props = defineProps<{
  instance: Instance
}>()

const emit = defineEmits<{
  close: []
}>()

const terminalElement = ref<HTMLDivElement | null>(null)
const status = ref('正在连接')
let socket: WebSocket | null = null
let terminal: Terminal | null = null
let fitAddon: FitAddon | null = null
let resizeObserver: ResizeObserver | null = null
let closedByUser = false

onMounted(() => {
  terminal = new Terminal({
    cursorBlink: true,
    cursorStyle: 'block',
    convertEol: false,
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

  terminal.onData((data) => {
    sendMessage({ type: 'input', data: encodeUtf8(data) })
  })
  terminal.onResize(({ cols, rows }) => {
    sendMessage({ type: 'resize', cols, rows })
  })

  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  socket = new WebSocket(
    `${protocol}//${window.location.host}/api/admin/instances/${props.instance.id}/terminal/ws`,
  )
  socket.onopen = () => {
    status.value = '正在启动交互式 Shell'
    fitTerminal()
    terminal?.focus()
  }
  socket.onmessage = (event) => {
    if (typeof event.data !== 'string') return
    try {
      const message = JSON.parse(event.data) as TerminalServerMessage
      switch (message.type) {
        case 'opening':
          status.value = '正在启动交互式 Shell'
          break
        case 'ready':
          status.value = '已连接'
          fitTerminal()
          terminal?.focus()
          break
        case 'output':
          terminal?.write(decodeBase64(message.data))
          break
        case 'closed':
          status.value = '会话已结束'
          terminal?.writeln(
            `\r\n\x1b[33m[终端已关闭${message.exit_code == null ? '' : `，退出码 ${message.exit_code}`}${message.reason ? `：${message.reason}` : ''}]\x1b[0m`,
          )
          break
        case 'error':
          status.value = '连接失败'
          terminal?.writeln(`\r\n\x1b[31m[${message.message}]\x1b[0m`)
          break
      }
    } catch {
      terminal?.writeln('\r\n\x1b[31m[收到无法解析的终端消息]\x1b[0m')
    }
  }
  socket.onerror = () => {
    status.value = '连接错误'
    terminal?.writeln('\r\n\x1b[31m[终端连接发生错误]\x1b[0m')
  }
  socket.onclose = () => {
    if (!closedByUser && status.value !== '会话已结束' && status.value !== '连接失败') {
      status.value = '连接已断开'
      terminal?.writeln('\r\n\x1b[33m[终端连接已断开]\x1b[0m')
    }
  }

  resizeObserver = new ResizeObserver(fitTerminal)
  resizeObserver.observe(terminalElement.value!)
  window.addEventListener('resize', fitTerminal)
  requestAnimationFrame(fitTerminal)
})

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

function fitTerminal() {
  if (!terminal || !fitAddon || !terminalElement.value) return
  try {
    fitAddon.fit()
  } catch {
    // The modal may be in the middle of unmounting.
  }
}

function sendMessage(message: TerminalClientMessage) {
  if (socket?.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(message))
  }
}

function encodeUtf8(value: string): string {
  const bytes = new TextEncoder().encode(value)
  let binary = ''
  for (let index = 0; index < bytes.length; index += 0x8000) {
    binary += String.fromCharCode(...bytes.subarray(index, index + 0x8000))
  }
  return btoa(binary)
}

function decodeBase64(value: string): Uint8Array {
  const binary = atob(value)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index)
  }
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
  <div class="modal-backdrop" @click.self="emit('close')">
    <section class="modal terminal-modal">
      <div class="terminal-head">
        <div>
          <h2>{{ instance.name }} · Web 终端</h2>
          <p>{{ status }} · UTF-8</p>
        </div>
        <button class="icon-button" type="button" title="关闭" @click="emit('close')">
          <X :size="17" />
        </button>
      </div>
      <div ref="terminalElement" class="terminal-screen" aria-label="交互式终端"></div>
    </section>
  </div>
</template>
