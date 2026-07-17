<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import {
  Expand,
  Fullscreen,
  Maximize2,
  Monitor,
  RefreshCw,
  ShieldAlert,
  Shrink,
  X,
} from 'lucide-vue-next'
import type { Instance } from '../types/domain'

const FRAME_HEADER_BYTES = 32
const MAX_FRAME_BYTES = 2 * 1024 * 1024
const POINTER_INTERVAL_MS = 1000 / 30
const IDLE_TIMEOUT_MS = 15 * 60 * 1000
const SESSION_TIMEOUT_MS = 2 * 60 * 60 * 1000

type ConnectionState = 'connecting' | 'ready' | 'paused' | 'closed' | 'error' | 'disconnected'
type ViewMode = 'fit' | 'actual'
type PointerButton = 0 | 1 | 2
type KeyModifier = 'alt' | 'ctrl' | 'shift' | 'meta'

type DesktopFrame = {
  generation: number
  sequence: number
  width: number
  height: number
  jpeg: ArrayBuffer
}

type DesktopServerMessage =
  | { type: 'opening' }
  | { type: 'ready' }
  | { type: 'display'; width: number; height: number }
  | { type: 'desktop_state'; desktop: 'default' | 'secure' | 'other' }
  | { type: 'notice'; code: string; message: string }
  | { type: 'paused'; reason: string }
  | { type: 'closed'; reason: string }
  | { type: 'error'; code: string; message: string }

type DesktopClientMessage =
  | { type: 'pointer_move'; x: number; y: number }
  | { type: 'pointer_button'; button: PointerButton; down: boolean; x: number; y: number }
  | { type: 'wheel'; delta_x: number; delta_y: number; x: number; y: number }
  | {
    type: 'key'
    code: string
    down: boolean
    repeat: boolean
    modifiers: KeyModifier[]
  }
  | { type: 'release_all' }
  | { type: 'secure_attention' }
  | {
    type: 'feedback'
    sequence: number
    fps: number
    decode_ms: number
  }

const props = defineProps<{
  instance: Instance
}>()

const emit = defineEmits<{
  close: []
}>()

const rootElement = ref<HTMLElement | null>(null)
const viewportElement = ref<HTMLDivElement | null>(null)
const canvasElement = ref<HTMLCanvasElement | null>(null)
const connectionState = ref<ConnectionState>('connecting')
const statusDetail = ref('正在建立安全连接')
const displayWidth = ref(0)
const displayHeight = ref(0)
const renderedFps = ref(0)
const viewMode = ref<ViewMode>('fit')
const isFullscreen = ref(false)
const isNarrowViewport = ref(false)
const hasCoarsePointer = ref(false)
const sessionWarning = ref('')
const desktopKind = ref<'default' | 'secure' | 'other'>('default')

let socket: WebSocket | null = null
let socketGeneration = 0
let resizeObserver: ResizeObserver | null = null
let viewportMediaQuery: MediaQueryList | null = null
let pointerMediaQuery: MediaQueryList | null = null
let feedbackTimer: number | null = null
let warningTimer: number | null = null
let reconnectTimer: number | null = null
let pendingFrame: DesktopFrame | null = null
let decodingFrame = false
let currentBitmap: ImageBitmap | null = null
let latestSequence = 0
let renderTimes: number[] = []
let decodeTimes: number[] = []
let pointerFrame = 0
let pendingPointer: { x: number; y: number } | null = null
let lastPointer: { x: number; y: number } | null = null
let lastPointerSentAt = 0
let sessionStartedAt = 0
let lastInputAt = 0
const pressedMouseButtons = new Set<PointerButton>()
let renderedRect = { x: 0, y: 0, width: 0, height: 0 }

const instanceName = computed(() => props.instance.name || props.instance.hostname || '未命名节点')
const resolutionLabel = computed(() =>
  displayWidth.value && displayHeight.value
    ? `${displayWidth.value} × ${displayHeight.value}`
    : '等待画面',
)
const statusLabel = computed(() => {
  switch (connectionState.value) {
    case 'connecting': return '连接中'
    case 'ready': return '已连接'
    case 'paused': return '画面已暂停'
    case 'closed': return '会话已结束'
    case 'error': return '连接失败'
    case 'disconnected': return '连接已断开'
  }
})
const showStatusOverlay = computed(() => connectionState.value !== 'ready')
const showInputHint = computed(() => isNarrowViewport.value || hasCoarsePointer.value)
const canControl = computed(() => connectionState.value === 'ready' && !hasCoarsePointer.value)

onMounted(() => {
  viewportMediaQuery = window.matchMedia('(max-width: 760px)')
  pointerMediaQuery = window.matchMedia('(pointer: coarse)')
  updateMediaState()
  viewportMediaQuery.addEventListener('change', updateMediaState)
  pointerMediaQuery.addEventListener('change', updateMediaState)
  document.addEventListener('fullscreenchange', updateFullscreenState)
  window.addEventListener('blur', releaseAllInputs)
  document.addEventListener('visibilitychange', handleVisibilityChange)

  if (viewportElement.value) {
    resizeObserver = new ResizeObserver(() => drawCurrentFrame())
    resizeObserver.observe(viewportElement.value)
  }

  feedbackTimer = window.setInterval(sendFeedback, 2000)
  warningTimer = window.setInterval(updateSessionWarning, 1000)
  connect()
})

onBeforeUnmount(() => {
  releaseAllInputs()
  socketGeneration += 1
  socket?.close(1000, 'client_closed')
  socket = null
  resizeObserver?.disconnect()
  viewportMediaQuery?.removeEventListener('change', updateMediaState)
  pointerMediaQuery?.removeEventListener('change', updateMediaState)
  document.removeEventListener('fullscreenchange', updateFullscreenState)
  window.removeEventListener('blur', releaseAllInputs)
  document.removeEventListener('visibilitychange', handleVisibilityChange)
  if (feedbackTimer !== null) window.clearInterval(feedbackTimer)
  if (warningTimer !== null) window.clearInterval(warningTimer)
  if (reconnectTimer !== null) window.clearTimeout(reconnectTimer)
  if (pointerFrame) cancelAnimationFrame(pointerFrame)
  currentBitmap?.close()
  currentBitmap = null
  pendingFrame = null
})

function connect() {
  if (reconnectTimer !== null) {
    window.clearTimeout(reconnectTimer)
    reconnectTimer = null
  }
  const previousSocket = socket
  if (previousSocket && previousSocket.readyState !== WebSocket.CLOSED) {
    socketGeneration += 1
    socket = null
    sendMessage({ type: 'release_all' }, previousSocket)
    previousSocket.close(1000, 'reconnecting')
    releaseLocalInputState()
    connectionState.value = 'connecting'
    statusDetail.value = '正在关闭旧会话'
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null
      openConnection()
    }, 500)
    return
  }

  openConnection()
}

function openConnection() {
  const generation = ++socketGeneration
  connectionState.value = 'connecting'
  statusDetail.value = '正在建立安全连接'
  sessionWarning.value = ''
  sessionStartedAt = 0
  lastInputAt = 0
  pendingFrame = null
  latestSequence = 0
  renderTimes = []
  decodeTimes = []
  renderedFps.value = 0
  pressedMouseButtons.clear()
  currentBitmap?.close()
  currentBitmap = null
  clearCanvas()

  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const id = encodeURIComponent(props.instance.id)
  const nextSocket = new WebSocket(`${protocol}//${window.location.host}/api/admin/instances/${id}/desktop/ws`)
  nextSocket.binaryType = 'arraybuffer'
  socket = nextSocket

  nextSocket.onopen = () => {
    if (generation !== socketGeneration) return
    statusDetail.value = '正在请求 Windows 交互会话'
  }
  nextSocket.onmessage = (event) => {
    if (generation !== socketGeneration) return
    if (typeof event.data === 'string') {
      handleServerMessage(event.data)
      return
    }
    if (event.data instanceof ArrayBuffer) {
      handleFrame(event.data)
      return
    }
    if (event.data instanceof Blob) {
      void event.data.arrayBuffer().then((buffer) => {
        if (generation === socketGeneration) handleFrame(buffer)
      })
    }
  }
  nextSocket.onerror = () => {
    if (generation !== socketGeneration) return
    connectionState.value = 'error'
    statusDetail.value = '远程桌面连接发生错误'
  }
  nextSocket.onclose = () => {
    if (generation !== socketGeneration) return
    socket = null
    releaseLocalInputState()
    if (!['closed', 'error'].includes(connectionState.value)) {
      connectionState.value = 'disconnected'
      statusDetail.value = '数据连接已断开，可尝试重新连接'
    }
  }
}

function handleServerMessage(payload: string) {
  let message: DesktopServerMessage
  try {
    message = JSON.parse(payload) as DesktopServerMessage
  } catch {
    failProtocol('收到无法解析的远程桌面消息')
    return
  }

  switch (message.type) {
    case 'opening':
      connectionState.value = 'connecting'
      statusDetail.value = '正在启动 Windows 桌面捕获'
      break
    case 'ready':
      connectionState.value = 'ready'
      statusDetail.value = '键盘和鼠标操作将发送到远程实例'
      if (!sessionStartedAt) {
        sessionStartedAt = Date.now()
        lastInputAt = sessionStartedAt
      }
      void nextTick(() => canvasElement.value?.focus())
      break
    case 'display':
      if (message.width > 0 && message.height > 0) {
        displayWidth.value = message.width
        displayHeight.value = message.height
        void nextTick(drawCurrentFrame)
      }
      break
    case 'desktop_state':
      desktopKind.value = message.desktop
      connectionState.value = 'ready'
      statusDetail.value = message.desktop === 'secure'
        ? '正在操作 Windows 登录或 UAC 安全桌面'
        : message.desktop === 'other'
          ? '正在操作 Windows 系统桌面'
          : '键盘和鼠标操作将发送到远程实例'
      break
    case 'notice':
      statusDetail.value = message.message || desktopError(message.code, '')
      break
    case 'paused':
      connectionState.value = 'paused'
      statusDetail.value = desktopReason(message.reason)
      releaseAllInputs()
      break
    case 'closed':
      connectionState.value = 'closed'
      statusDetail.value = desktopReason(message.reason) || '远程桌面会话已结束'
      releaseLocalInputState()
      break
    case 'error':
      connectionState.value = 'error'
      statusDetail.value = desktopError(message.code, message.message)
      releaseLocalInputState()
      break
    default:
      failProtocol('收到未知的远程桌面消息')
  }
}

function handleFrame(buffer: ArrayBuffer) {
  if (buffer.byteLength <= FRAME_HEADER_BYTES || buffer.byteLength > MAX_FRAME_BYTES) {
    failProtocol('收到大小异常的桌面画面')
    return
  }

  const bytes = new Uint8Array(buffer, 0, 4)
  if (bytes[0] !== 0x4f || bytes[1] !== 0x4d || bytes[2] !== 0x52 || bytes[3] !== 0x44) {
    failProtocol('收到无效的桌面画面标识')
    return
  }

  const view = new DataView(buffer)
  if (view.getUint8(4) !== 1 || view.getUint8(5) !== 1 || view.getUint16(6, false) !== 0) {
    failProtocol('当前浏览器不支持此桌面画面版本或编码')
    return
  }

  const width = view.getUint32(24, false)
  const height = view.getUint32(28, false)
  if (!width || !height || width > 1920 || height > 1080) {
    failProtocol('收到无效的桌面画面尺寸')
    return
  }

  const sequence = Number(view.getBigUint64(8, false))
  pendingFrame = {
    generation: socketGeneration,
    sequence,
    width,
    height,
    jpeg: buffer.slice(FRAME_HEADER_BYTES),
  }
  displayWidth.value = width
  displayHeight.value = height
  if (!decodingFrame) void decodeNextFrame()
}

async function decodeNextFrame() {
  decodingFrame = true
  while (pendingFrame) {
    const frame = pendingFrame
    pendingFrame = null
    const startedAt = performance.now()
    try {
      const bitmap = await createImageBitmap(new Blob([frame.jpeg], { type: 'image/jpeg' }))
      if (frame.generation !== socketGeneration) {
        bitmap.close()
        continue
      }
      decodeTimes.push(performance.now() - startedAt)
      if (decodeTimes.length > 120) decodeTimes.shift()

      const previousBitmap = currentBitmap
      currentBitmap = bitmap
      latestSequence = frame.sequence
      drawCurrentFrame()
      previousBitmap?.close()

      const now = performance.now()
      renderTimes.push(now)
      renderTimes = renderTimes.filter((value) => value >= now - 1000)
      renderedFps.value = renderTimes.length
    } catch {
      if (frame.generation !== socketGeneration) continue
      failProtocol('浏览器无法解码远程桌面画面')
      pendingFrame = null
    }
  }
  decodingFrame = false
}

function drawCurrentFrame() {
  const canvas = canvasElement.value
  const viewport = viewportElement.value
  const bitmap = currentBitmap
  if (!canvas || !viewport || !bitmap) return

  if (viewMode.value === 'actual') {
    canvas.style.width = `${displayWidth.value || bitmap.width}px`
    canvas.style.height = `${displayHeight.value || bitmap.height}px`
  } else {
    canvas.style.width = '100%'
    canvas.style.height = '100%'
  }

  const cssWidth = Math.max(1, canvas.clientWidth)
  const cssHeight = Math.max(1, canvas.clientHeight)
  const pixelRatio = Math.min(window.devicePixelRatio || 1, 2)
  const targetWidth = Math.round(cssWidth * pixelRatio)
  const targetHeight = Math.round(cssHeight * pixelRatio)
  if (canvas.width !== targetWidth) canvas.width = targetWidth
  if (canvas.height !== targetHeight) canvas.height = targetHeight

  const context = canvas.getContext('2d')
  if (!context) return
  context.setTransform(1, 0, 0, 1, 0, 0)
  context.fillStyle = '#050607'
  context.fillRect(0, 0, targetWidth, targetHeight)

  const sourceWidth = displayWidth.value || bitmap.width
  const sourceHeight = displayHeight.value || bitmap.height
  const scale = viewMode.value === 'fit'
    ? Math.min(cssWidth / sourceWidth, cssHeight / sourceHeight)
    : 1
  const drawWidth = sourceWidth * scale
  const drawHeight = sourceHeight * scale
  const drawX = Math.max(0, (cssWidth - drawWidth) / 2)
  const drawY = Math.max(0, (cssHeight - drawHeight) / 2)
  renderedRect = { x: drawX, y: drawY, width: drawWidth, height: drawHeight }
  context.drawImage(
    bitmap,
    drawX * pixelRatio,
    drawY * pixelRatio,
    drawWidth * pixelRatio,
    drawHeight * pixelRatio,
  )
}

function clearCanvas() {
  const canvas = canvasElement.value
  const context = canvas?.getContext('2d')
  if (!canvas || !context) return
  context.clearRect(0, 0, canvas.width, canvas.height)
}

function setViewMode(mode: ViewMode) {
  viewMode.value = mode
  void nextTick(drawCurrentFrame)
}

async function toggleFullscreen() {
  try {
    if (document.fullscreenElement) {
      await document.exitFullscreen()
    } else {
      await rootElement.value?.requestFullscreen()
    }
  } catch {
    statusDetail.value = '浏览器未允许进入全屏模式'
  }
}

function updateFullscreenState() {
  isFullscreen.value = document.fullscreenElement === rootElement.value
  void nextTick(drawCurrentFrame)
}

function normalizedPointer(event: PointerEvent | WheelEvent) {
  const canvas = canvasElement.value
  if (!canvas || renderedRect.width <= 0 || renderedRect.height <= 0) return null
  const bounds = canvas.getBoundingClientRect()
  const localX = event.clientX - bounds.left
  const localY = event.clientY - bounds.top
  if (
    localX < renderedRect.x
    || localY < renderedRect.y
    || localX > renderedRect.x + renderedRect.width
    || localY > renderedRect.y + renderedRect.height
  ) return null

  return {
    x: Math.max(0, Math.min(1, (localX - renderedRect.x) / renderedRect.width)),
    y: Math.max(0, Math.min(1, (localY - renderedRect.y) / renderedRect.height)),
  }
}

function handlePointerMove(event: PointerEvent) {
  if (event.pointerType === 'touch' || !canControl.value) return
  const point = normalizedPointer(event)
  if (!point) return
  lastPointer = point
  pendingPointer = point
  if (!pointerFrame) pointerFrame = requestAnimationFrame(flushPointerMove)
}

function flushPointerMove(timestamp: number) {
  pointerFrame = 0
  if (!pendingPointer) return
  if (timestamp - lastPointerSentAt < POINTER_INTERVAL_MS) {
    pointerFrame = requestAnimationFrame(flushPointerMove)
    return
  }
  const point = pendingPointer
  pendingPointer = null
  lastPointerSentAt = timestamp
  if (sendMessage({ type: 'pointer_move', ...point })) markInputActivity()
}

function handlePointerButton(event: PointerEvent, pressed: boolean) {
  if (event.pointerType === 'touch' || !canControl.value) return
  const button = pointerButton(event.button)
  if (button === null) return
  const point = normalizedPointer(event)
  if (!point && (pressed || !pressedMouseButtons.has(button))) return
  const target = point || lastPointer
  if (!target) return
  event.preventDefault()
  canvasElement.value?.focus()
  if (pressed) {
    pressedMouseButtons.add(button)
    canvasElement.value?.setPointerCapture(event.pointerId)
  } else {
    pressedMouseButtons.delete(button)
    if (canvasElement.value?.hasPointerCapture(event.pointerId)) {
      canvasElement.value.releasePointerCapture(event.pointerId)
    }
  }
  lastPointer = target
  if (sendMessage({ type: 'pointer_button', button, down: pressed, ...target })) markInputActivity()
}

function handlePointerCancel() {
  releaseAllInputs()
}

function handleWheel(event: WheelEvent) {
  if (!canControl.value) return
  const point = normalizedPointer(event)
  if (!point) return
  event.preventDefault()
  if (sendMessage({
    type: 'wheel',
    delta_x: normalizeWheelDelta(event.deltaX, event.deltaMode),
    delta_y: normalizeWheelDelta(event.deltaY, event.deltaMode),
    ...point,
  })) markInputActivity()
}

function handleKey(event: KeyboardEvent, pressed: boolean) {
  if (!canControl.value || event.isComposing || !event.code) return
  event.preventDefault()
  if (sendMessage({
    type: 'key',
    code: event.code,
    down: pressed,
    repeat: event.repeat,
    modifiers: keyModifiers(event),
  })) markInputActivity()
}

function releaseAllInputs() {
  if (socket?.readyState === WebSocket.OPEN) sendMessage({ type: 'release_all' })
  releaseLocalInputState()
}

function sendSecureAttention() {
  if (!canControl.value) return
  if (!window.confirm('确定向远程 Windows 发送 Ctrl+Alt+Del 吗？')) return
  releaseAllInputs()
  if (sendMessage({ type: 'secure_attention' })) {
    markInputActivity()
    statusDetail.value = '已请求 Windows 显示安全选项'
  }
}

function releaseLocalInputState() {
  pressedMouseButtons.clear()
  pendingPointer = null
  lastPointer = null
}

function pointerButton(button: number): PointerButton | null {
  if (button === 0 || button === 1 || button === 2) return button
  return null
}

function keyModifiers(event: KeyboardEvent): KeyModifier[] {
  const modifiers: KeyModifier[] = []
  if (event.altKey) modifiers.push('alt')
  if (event.ctrlKey) modifiers.push('ctrl')
  if (event.shiftKey) modifiers.push('shift')
  if (event.metaKey) modifiers.push('meta')
  return modifiers
}

function toI32(value: number) {
  return Math.max(-100_000, Math.min(100_000, Math.round(value)))
}

function normalizeWheelDelta(value: number, mode: number) {
  const scale = mode === WheelEvent.DOM_DELTA_LINE
    ? 40
    : mode === WheelEvent.DOM_DELTA_PAGE
      ? viewportElement.value?.clientHeight || window.innerHeight
      : 1
  return toI32(value * scale)
}

function sendMessage(message: DesktopClientMessage, target = socket) {
  if (target?.readyState !== WebSocket.OPEN) return false
  target.send(JSON.stringify(message))
  return true
}

function sendFeedback() {
  const now = performance.now()
  renderTimes = renderTimes.filter((value) => value >= now - 1000)
  renderedFps.value = renderTimes.length
  if (connectionState.value !== 'ready') return
  const averageDecodeMs = decodeTimes.length
    ? decodeTimes.reduce((sum, value) => sum + value, 0) / decodeTimes.length
    : 0
  sendMessage({
    type: 'feedback',
    sequence: latestSequence,
    fps: renderedFps.value,
    decode_ms: Math.round(averageDecodeMs * 10) / 10,
  })
}

function markInputActivity() {
  lastInputAt = Date.now()
  sessionWarning.value = ''
}

function updateSessionWarning() {
  if (connectionState.value !== 'ready' || !sessionStartedAt || !lastInputAt) {
    sessionWarning.value = ''
    return
  }
  const now = Date.now()
  const idleRemaining = IDLE_TIMEOUT_MS - (now - lastInputAt)
  const sessionRemaining = SESSION_TIMEOUT_MS - (now - sessionStartedAt)
  const remaining = Math.min(idleRemaining, sessionRemaining)
  if (remaining > 0 && remaining <= 60_000) {
    const cause = idleRemaining <= sessionRemaining ? '长时间无操作' : '达到最长时长'
    sessionWarning.value = `会话将在 ${Math.ceil(remaining / 1000)} 秒后因${cause}结束`
  } else {
    sessionWarning.value = ''
  }
}

function failProtocol(message: string) {
  connectionState.value = 'error'
  statusDetail.value = message
  releaseAllInputs()
  socket?.close(1002, 'invalid_desktop_message')
}

function desktopError(code: string, fallback: string) {
  const errors: Record<string, string> = {
    desktop_busy: '该实例已有管理员正在操作远程桌面',
    no_active_session: 'Windows 当前没有已登录的活动用户会话',
    multiple_active_sessions: 'Windows 存在多个活动用户会话，暂时无法自动选择',
    desktop_locked: 'Windows 桌面已锁定，解锁后可继续操作',
    secure_desktop: 'Windows 正在显示安全桌面，暂不支持远程操作',
    secure_attention_unavailable: 'Windows 或系统策略未允许发送 Ctrl+Alt+Del',
    unsupported: '当前 Agent 不支持网页远程桌面',
    instance_offline: '实例当前离线，无法建立远程桌面连接',
    unauthorized: '管理员登录已失效，请重新登录',
  }
  return errors[code] || fallback || '远程桌面连接失败'
}

function desktopReason(reason: string) {
  const reasons: Record<string, string> = {
    no_active_session: 'Windows 当前没有已登录的活动用户会话',
    multiple_active_sessions: 'Windows 存在多个活动用户会话，暂时无法自动选择',
    desktop_locked: 'Windows 桌面已锁定，解锁后将自动继续',
    secure_desktop: 'Windows 正在显示 UAC 或其他安全桌面，返回普通桌面后将自动继续',
    secure_desktop_requires_service: '安全桌面控制需要安装并运行 Windows 系统服务',
    logged_out: 'Windows 用户已注销，远程桌面会话已结束',
    idle_timeout: '会话因长时间无键鼠操作而结束',
    session_timeout: '会话已达到最长 2 小时时长',
    browser_disconnected: '浏览器连接已断开',
    agent_disconnected: 'Agent 已断开连接',
    client_closed: '远程桌面会话已关闭',
    browser_closed: '浏览器已关闭远程桌面连接',
    desktop_busy: '该实例已有管理员正在操作远程桌面',
    agent_draining: 'Agent 正在准备更新，暂不接受新的远程桌面会话',
    unsupported_platform: '该实例平台不支持远程桌面',
    helper_disconnected: 'Windows 桌面捕获进程已断开',
    helper_error: 'Windows 桌面捕获进程发生错误，请查看 Agent 日志',
    agent_data_error: 'Agent 远程桌面数据连接发生错误',
    agent_error: 'Agent 处理远程桌面时发生错误，请查看 Agent 日志',
    frame_too_large: '桌面画面复杂度过高，JPEG 帧超过 2 MiB 限制',
  }
  return reasons[reason] || reason || '远程桌面会话已结束'
}

function updateMediaState() {
  isNarrowViewport.value = viewportMediaQuery?.matches ?? window.innerWidth <= 760
  hasCoarsePointer.value = pointerMediaQuery?.matches ?? false
  if (hasCoarsePointer.value) releaseAllInputs()
}

function handleVisibilityChange() {
  if (document.hidden) releaseAllInputs()
}
</script>

<template>
  <div ref="rootElement" class="remote-desktop-backdrop">
    <section class="remote-desktop-modal" role="dialog" aria-modal="true" aria-labelledby="remote-desktop-title">
      <header class="remote-desktop-head">
        <div class="remote-desktop-title">
          <span><Monitor :size="18" /></span>
          <div>
            <h2 id="remote-desktop-title">{{ instanceName }} · 远程桌面</h2>
            <p>
              <i :class="connectionState"></i>{{ statusLabel }}
              <span>{{ resolutionLabel }}</span>
              <span>{{ renderedFps }} FPS</span>
              <span v-if="desktopKind !== 'default'">{{ desktopKind === 'secure' ? '安全桌面' : '系统桌面' }}</span>
            </p>
          </div>
        </div>

        <div class="remote-desktop-tools">
          <div class="desktop-view-toggle" role="group" aria-label="远程桌面缩放模式">
            <button
              :class="{ active: viewMode === 'fit' }"
              type="button"
              title="使桌面画面适应窗口"
              :aria-pressed="viewMode === 'fit'"
              @click="setViewMode('fit')"
            >
              <Shrink :size="15" />适应窗口
            </button>
            <button
              :class="{ active: viewMode === 'actual' }"
              type="button"
              title="按原始像素显示桌面"
              :aria-pressed="viewMode === 'actual'"
              @click="setViewMode('actual')"
            >
              <Maximize2 :size="15" />1:1
            </button>
          </div>
          <button
            class="desktop-tool-button desktop-secure-button"
            type="button"
            title="向远程 Windows 发送 Ctrl+Alt+Del"
            :disabled="!canControl"
            @click="sendSecureAttention"
          >
            <ShieldAlert :size="15" /><span>Ctrl+Alt+Del</span>
          </button>
          <button class="desktop-tool-button" type="button" title="重新连接" @click="connect">
            <RefreshCw :size="15" /><span>重连</span>
          </button>
          <button class="desktop-tool-button" type="button" title="切换浏览器全屏" @click="toggleFullscreen">
            <Fullscreen v-if="!isFullscreen" :size="15" />
            <Expand v-else :size="15" />
            <span>{{ isFullscreen ? '退出全屏' : '全屏' }}</span>
          </button>
          <button class="desktop-close-button" type="button" title="关闭远程桌面" aria-label="关闭远程桌面" @click="emit('close')">
            <X :size="18" />
          </button>
        </div>
      </header>

      <div ref="viewportElement" :class="['remote-desktop-viewport', viewMode]">
        <canvas
          ref="canvasElement"
          class="remote-desktop-canvas"
          tabindex="0"
          aria-label="可通过键盘和鼠标操作的 Windows 远程桌面"
          @pointermove="handlePointerMove"
          @pointerdown="handlePointerButton($event, true)"
          @pointerup="handlePointerButton($event, false)"
          @pointercancel="handlePointerCancel"
          @wheel="handleWheel"
          @keydown="handleKey($event, true)"
          @keyup="handleKey($event, false)"
          @blur="releaseAllInputs"
          @contextmenu.prevent
        ></canvas>

        <Transition name="fade-scale">
          <div v-if="showStatusOverlay" :class="['remote-desktop-status', connectionState]" role="status">
            <span><Monitor :size="30" /></span>
            <strong>{{ statusLabel }}</strong>
            <p>{{ statusDetail }}</p>
            <button
              v-if="connectionState === 'error' || connectionState === 'closed' || connectionState === 'disconnected'"
              type="button"
              @click="connect"
            >
              <RefreshCw :size="14" />重新连接
            </button>
          </div>
        </Transition>

        <div v-if="showInputHint" class="remote-desktop-input-hint">
          窄屏可查看画面；首版远程操作需要桌面端鼠标和键盘，不支持触控模拟。
        </div>
        <div v-if="sessionWarning" class="remote-desktop-session-warning" role="alert">
          {{ sessionWarning }}
        </div>
      </div>
    </section>
  </div>
</template>
