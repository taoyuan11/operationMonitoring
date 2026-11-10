<script setup lang="ts">
import UiButton from './ui/UiButton.vue'
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { FitAddon } from '@xterm/addon-fit'
import { Terminal } from '@xterm/xterm'
import '@xterm/xterm/css/xterm.css'
import { TerminalSquare, X } from '@lucide/vue'
import { dockerWebSocketUrl } from '../api/docker'
import { createTerminalInput } from '../utils/terminalInput'
import type { DockerTerminalServerMessage } from '../types/docker'

const props = defineProps<{
  instanceId: string
  containerId: string
  containerName: string
  shell: '/bin/sh' | '/bin/bash' | '/bin/ash'
}>()

const emit = defineEmits<{ close: [] }>()

const terminalElement = ref<HTMLDivElement | null>(null)
const status = ref('正在连接')
let socket: WebSocket | null = null
let terminal: Terminal | null = null
let fitAddon: FitAddon | null = null
let resizeObserver: ResizeObserver | null = null
let closedByUser = false
const input = createTerminalInput(() => socket)

onMounted(() => {
  terminal = new Terminal({
    cursorBlink: true,
    cursorStyle: 'block',
    convertEol: true,
    fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace',
    fontSize: 13,
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
    if (!input.enqueue(data)) terminal?.writeln('\r\n\x1b[33m[待发送输入过多，本次输入未发送，请等待后重试]\x1b[0m')
  })
  terminal.onResize(({ cols, rows }) => input.resize(cols, rows))

  socket = new WebSocket(dockerWebSocketUrl(
    props.instanceId,
    `containers/${encodeURIComponent(props.containerId)}/exec/ws`,
    { shell: props.shell },
  ))
  socket.onopen = () => {
    status.value = '正在启动'
    fitTerminal()
  }
  socket.onmessage = (event) => handleMessage(event.data)
  socket.onerror = () => {
    input.stop()
    status.value = '连接错误'
    terminal?.writeln('\r\n\x1b[31m[容器终端连接发生错误]\x1b[0m')
  }
  socket.onclose = () => {
    input.stop()
    if (!closedByUser && status.value !== '会话已结束' && status.value !== '连接失败') {
      status.value = '连接已断开'
      terminal?.writeln('\r\n\x1b[33m[容器终端连接已断开]\x1b[0m')
    }
  }

  resizeObserver = new ResizeObserver(fitTerminal)
  resizeObserver.observe(terminalElement.value!)
  window.addEventListener('resize', fitTerminal)
  requestAnimationFrame(fitTerminal)
})

onBeforeUnmount(() => {
  closedByUser = true
  input.stop()
  resizeObserver?.disconnect()
  window.removeEventListener('resize', fitTerminal)
  socket?.close()
  terminal?.dispose()
  socket = null
  terminal = null
  fitAddon = null
})

function handleMessage(raw: unknown) {
  if (typeof raw !== 'string') return
  try {
    const message = JSON.parse(raw) as DockerTerminalServerMessage
    if (message.type === 'opening') status.value = '正在启动'
    if (message.type === 'ready') {
      if (terminal) input.resize(terminal.cols, terminal.rows)
      input.start(message.input_ack === true)
      status.value = '已连接'
      fitTerminal()
      terminal?.focus()
    }
    if (message.type === 'input_ack') input.acknowledge(message.input_id)
    if (message.type === 'output') {
      terminal?.write(message.encoding === 'utf8' ? message.data : decodeBase64(message.data))
    }
    if (message.type === 'closed') {
      input.stop()
      status.value = '会话已结束'
      terminal?.writeln(
        `\r\n\x1b[33m[终端已关闭${message.exit_code == null ? '' : `，退出码 ${message.exit_code}`}${message.reason ? `：${message.reason}` : ''}]\x1b[0m`,
      )
    }
    if (message.type === 'error') {
      input.stop()
      status.value = '连接失败'
      terminal?.writeln(`\r\n\x1b[31m[${message.message}]\x1b[0m`)
    }
  } catch {
    terminal?.writeln('\r\n\x1b[31m[收到无法解析的终端消息]\x1b[0m')
  }
}

function fitTerminal() {
  if (!terminal || !fitAddon || !terminalElement.value) return
  try {
    fitAddon.fit()
  } catch {
    // The overlay may be unmounting.
  }
}

function decodeBase64(value: string) {
  const binary = atob(value)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index)
  return bytes
}
</script>

<template>
  <div class="docker-overlay">
    <section class="docker-stream-window docker-terminal-window" role="dialog" aria-modal="true" aria-label="容器终端">
      <header class="docker-stream-head">
        <div class="docker-stream-title">
          <TerminalSquare :size="17" />
          <div><strong>{{ containerName }}</strong><span>{{ shell }} · {{ status }}</span></div>
        </div>
        <UiButton class="icon-button subtle" type="button" v-tooltip="'关闭终端'" @click="emit('close')">
          <X :size="16" />
        </UiButton>
      </header>
      <div class="terminal-screen docker-terminal-screen" aria-label="容器交互式终端">
        <div ref="terminalElement" class="terminal-screen-content"></div>
      </div>
    </section>
  </div>
</template>
