<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import {
  Expand,
  Fullscreen,
  Gauge,
  Keyboard,
  LoaderCircle,
  Maximize2,
  Monitor,
  RefreshCw,
  Shrink,
  Volume2,
  VolumeX,
  X,
} from '@lucide/vue'
import {
  DESKTOP_AUDIO_CAPABILITY,
  canSendDesktopSecureAttention,
  desktopMessageControllable,
  desktopStateAllowsAudio,
  desktopWebSocketUrl,
  parseDesktopMediaFrame,
  parseDesktopServerMessage,
  resolveDesktopInteractionState,
  type DesktopContext,
  type DesktopDisplaySource,
  type DesktopDisplayState,
  type DesktopKind,
  type DesktopQuality,
  type DesktopServerMessage,
  type DesktopVideoFrame,
} from '../api/desktop'
import { useRemoteDesktopAudio } from '../composables/useRemoteDesktopAudio'
import type { Instance } from '../types/domain'

const POINTER_INTERVAL_MS = 1000 / 30
const IDLE_TIMEOUT_MS = 15 * 60 * 1000
const SESSION_TIMEOUT_MS = 2 * 60 * 60 * 1000
const DESKTOP_QUALITY_STORAGE_KEY = 'operation-monitoring.desktop-quality'

type ConnectionState = 'connecting' | 'ready' | 'paused' | 'closed' | 'error' | 'disconnected'
type ViewMode = 'fit' | 'actual'
type PointerButton = 0 | 1 | 2
type KeyModifier = 'alt' | 'ctrl' | 'shift' | 'meta'

const desktopQualityOptions: Array<{ value: DesktopQuality; label: string }> = [
  { value: 'low', label: '省流 540p' },
  { value: 'balanced', label: '均衡 720p' },
  { value: 'high', label: '清晰 900p' },
  { value: 'original', label: '原画 1080p' },
]

type DesktopFrame = DesktopVideoFrame & {
  generation: number
  displayGeneration: number
}

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
  | { type: 'audio_control'; enabled: boolean }
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
const desktopQuality = ref<DesktopQuality>(readDesktopQuality())
const isFullscreen = ref(false)
const isNarrowViewport = ref(false)
const hasCoarsePointer = ref(false)
const sessionWarning = ref('')
const desktopNotice = ref('')
const desktopKind = ref<DesktopKind>('default')
const desktopContext = ref<DesktopContext>('default')
const desktopControllable = ref(true)
const displayState = ref<DesktopDisplayState>('unknown')
const displaySource = ref<DesktopDisplaySource>('unknown')
const sessionAccessMode = ref<'local_consent' | 'unattended'>('local_consent')
const secureDesktopControl = ref(false)
const secureAttentionAllowed = ref(false)
const serverReady = ref(false)
const firstFrameRendered = ref(false)

let socket: WebSocket | null = null
let socketGeneration = 0
let displayGeneration = 0
let resizeObserver: ResizeObserver | null = null
let viewportMediaQuery: MediaQueryList | null = null
let pointerMediaQuery: MediaQueryList | null = null
let feedbackTimer: number | null = null
let warningTimer: number | null = null
let reconnectTimer: number | null = null
let noticeTimer: number | null = null
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
let componentActive = true
let audioSupportReady = false
let connectQueued = false
const pressedMouseButtons = new Set<PointerButton>()
let renderedRect = { x: 0, y: 0, width: 0, height: 0 }

const {
  canToggle: canToggleAudio,
  enabled: audioEnabled,
  negotiated: audioNegotiated,
  requestedCodec: requestedAudioCodec,
  title: audioTitle,
  dispose: disposeRemoteAudio,
  handleConnectionPaused: pauseRemoteAudio,
  handleConnectionReady: readyRemoteAudio,
  handleFrame: handleAudioFrame,
  handleOpening: openRemoteAudio,
  handleServerState: handleAudioState,
  prepare: prepareRemoteAudio,
  resetConnection: resetRemoteAudio,
  toggle: toggleRemoteAudio,
} = useRemoteDesktopAudio({
  agentSupported: props.instance.capabilities?.includes(DESKTOP_AUDIO_CAPABILITY) === true,
  sendControl: (enabled) => sendMessage({ type: 'audio_control', enabled }),
})

const instanceName = computed(() => props.instance.name || props.instance.hostname || '未命名节点')
const resolutionLabel = computed(() =>
  displayWidth.value && displayHeight.value
    ? `${displayWidth.value} × ${displayHeight.value}`
    : '等待画面',
)
const statusLabel = computed(() => {
  if (connectionState.value === 'connecting' && displayState.value === 'preparing') return '准备显示器'
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
const canControl = computed(() =>
  connectionState.value === 'ready' && desktopControllable.value && !hasCoarsePointer.value,
)
const canSendSecureAttention = computed(() => canSendDesktopSecureAttention({
  accessMode: sessionAccessMode.value,
  secureDesktopControl: secureDesktopControl.value,
  secureAttentionAllowed: secureAttentionAllowed.value,
  desktopControllable: desktopControllable.value,
  serverReady: serverReady.value,
  firstFrameRendered: firstFrameRendered.value,
}))
const secureAttentionTitle = computed(() => {
  if (canSendSecureAttention.value) return '向远程 Windows 发送 Ctrl+Alt+Del'
  if (sessionAccessMode.value !== 'unattended') return '当前会话需要本地同意，不能发送 Ctrl+Alt+Del'
  if (!secureDesktopControl.value) return '当前 Agent 不能控制 Windows 安全桌面'
  return '当前桌面尚不能接收 Ctrl+Alt+Del'
})
const displaySourceLabel = computed(() => {
  if (displaySource.value === 'physical') return '物理显示器'
  if (displaySource.value === 'virtual') return '虚拟显示器'
  return ''
})

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
  connectQueued = true
  void prepareRemoteAudio().then(
    finishAudioPreparation,
    finishAudioPreparation,
  )
})

onBeforeUnmount(() => {
  componentActive = false
  disposeRemoteAudio()
  releaseAllInputs()
  socketGeneration += 1
  socket?.close(1000, 'client_closed')
  socket = null
  if (noticeTimer !== null) window.clearTimeout(noticeTimer)
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
  if (!componentActive) return
  if (!audioSupportReady) {
    connectQueued = true
    statusDetail.value = '正在检查浏览器音频支持'
    return
  }
  connectQueued = false
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
    resetRemoteAudio(true)
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

function finishAudioPreparation() {
  audioSupportReady = true
  if (componentActive && connectQueued) connect()
}

function openConnection() {
  if (!componentActive) return
  const generation = ++socketGeneration
  resetRemoteAudio(true)
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
  desktopKind.value = 'default'
  desktopContext.value = 'default'
  desktopControllable.value = true
  displayState.value = 'unknown'
  displaySource.value = 'unknown'
  sessionAccessMode.value = 'local_consent'
  secureDesktopControl.value = false
  secureAttentionAllowed.value = false
  serverReady.value = false
  firstFrameRendered.value = false
  pressedMouseButtons.clear()
  currentBitmap?.close()
  currentBitmap = null
  clearCanvas()

  const nextSocket = new WebSocket(
    desktopWebSocketUrl(props.instance.id, desktopQuality.value, requestedAudioCodec.value),
  )
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
      handleMediaFrame(event.data)
      return
    }
    if (event.data instanceof Blob) {
      void event.data.arrayBuffer().then((buffer) => {
        if (generation === socketGeneration) handleMediaFrame(buffer)
      })
    }
  }
  nextSocket.onerror = () => {
    if (generation !== socketGeneration) return
    connectionState.value = 'error'
    statusDetail.value = '远程桌面连接发生错误'
    pauseRemoteAudio()
  }
  nextSocket.onclose = () => {
    if (generation !== socketGeneration) return
    socket = null
    releaseLocalInputState()
    pauseRemoteAudio()
    if (!['closed', 'error'].includes(connectionState.value)) {
      connectionState.value = 'disconnected'
      statusDetail.value = '数据连接已断开，可尝试重新连接'
    }
  }
}

function handleServerMessage(payload: string) {
  let message: DesktopServerMessage
  try {
    message = parseDesktopServerMessage(payload)
  } catch (error) {
    failProtocol(error instanceof Error ? error.message : '收到无法解析的远程桌面消息')
    return
  }

  switch (message.type) {
    case 'opening':
      openRemoteAudio(message.audio_codec)
      connectionState.value = 'connecting'
      statusDetail.value = '正在启动 Windows 桌面捕获'
      break
    case 'consent_required':
      pauseRemoteAudio()
      connectionState.value = 'connecting'
      statusDetail.value = '等待远程计算机上的用户允许本次查看和控制'
      break
    case 'ready':
      serverReady.value = true
      updateInteractiveState()
      break
    case 'display':
      if (message.width > 0 && message.height > 0) {
        displayWidth.value = message.width
        displayHeight.value = message.height
        void nextTick(drawCurrentFrame)
      }
      break
    case 'session_policy':
      sessionAccessMode.value = message.access_mode
      secureDesktopControl.value = message.secure_desktop_control
      secureAttentionAllowed.value = message.secure_attention_allowed
      if (message.local_consent_required) {
        statusDetail.value = '当前远程桌面会话需要本地用户同意'
      }
      break
    case 'display_state':
      displayState.value = message.state
      displaySource.value = message.source
      if (message.state === 'preparing') {
        displayGeneration += 1
        pendingFrame = null
        serverReady.value = false
        firstFrameRendered.value = false
        pauseRemoteAudio()
        releaseAllInputs()
        connectionState.value = 'connecting'
        statusDetail.value = message.source === 'virtual'
          ? '正在准备虚拟显示器'
          : '正在准备远程显示器'
      } else if (message.state === 'unavailable') {
        pauseRemoteAudio()
        releaseAllInputs()
        connectionState.value = 'error'
        statusDetail.value = desktopError(message.code || 'display_unavailable', '')
      } else {
        updateInteractiveState()
      }
      break
    case 'desktop_state':
      if (
        message.desktop !== desktopKind.value
        || (message.context !== undefined && message.context !== desktopContext.value)
      ) {
        displayGeneration += 1
        pendingFrame = null
        serverReady.value = false
        firstFrameRendered.value = false
        releaseAllInputs()
      }
      desktopKind.value = message.desktop
      desktopContext.value = message.context
        || (message.desktop === 'default' ? 'default' : message.desktop === 'secure' ? 'winlogon' : 'other')
      desktopControllable.value = desktopMessageControllable(message)
      if (!desktopStateAllowsAudio(message.desktop)) pauseRemoteAudio()
      updateInteractiveState()
      break
    case 'notice':
      statusDetail.value = desktopError(message.code, message.message)
      desktopNotice.value = statusDetail.value
      if (noticeTimer !== null) window.clearTimeout(noticeTimer)
      noticeTimer = window.setTimeout(() => {
        desktopNotice.value = ''
        noticeTimer = null
      }, 5000)
      break
    case 'audio_state':
      handleAudioState(message.state, message.reason)
      break
    case 'paused':
      pauseRemoteAudio()
      connectionState.value = 'paused'
      statusDetail.value = desktopReason(message.reason)
      releaseAllInputs()
      break
    case 'closed':
      pauseRemoteAudio()
      connectionState.value = 'closed'
      statusDetail.value = desktopReason(message.reason) || '远程桌面会话已结束'
      releaseLocalInputState()
      break
    case 'error':
      pauseRemoteAudio()
      connectionState.value = 'error'
      statusDetail.value = desktopError(message.code, message.message)
      releaseLocalInputState()
      break
    default:
      failProtocol('收到未知的远程桌面消息')
  }
}

function updateInteractiveState() {
  const interactionState = resolveDesktopInteractionState({
    displayState: displayState.value,
    serverReady: serverReady.value,
    firstFrameRendered: firstFrameRendered.value,
    desktopControllable: desktopControllable.value,
  })
  if (interactionState === 'preparing' || interactionState === 'unavailable') return
  if (interactionState === 'waiting_ready' || interactionState === 'waiting_frame') {
    connectionState.value = 'connecting'
    statusDetail.value = interactionState === 'waiting_frame'
      ? '正在等待远程桌面的首个有效画面'
      : '正在启动 Windows 桌面捕获'
    return
  }
  if (interactionState === 'paused') {
    connectionState.value = 'paused'
    statusDetail.value = desktopKind.value === 'secure'
      ? 'Windows 正在显示登录或 UAC 安全桌面，当前模式已暂停远程控制'
      : 'Windows 已切换到不可控制的系统桌面'
    releaseAllInputs()
    return
  }

  connectionState.value = 'ready'
  statusDetail.value = desktopContext.value === 'winlogon'
    ? '正在控制 Windows 登录或安全桌面'
    : '键盘和鼠标操作将发送到远程实例'
  if (desktopStateAllowsAudio(desktopKind.value)) readyRemoteAudio()
  if (!sessionStartedAt) {
    sessionStartedAt = Date.now()
    lastInputAt = sessionStartedAt
  }
  void nextTick(() => canvasElement.value?.focus())
}

function sendSecureAttention() {
  if (!canSendSecureAttention.value) return
  if (sendMessage({ type: 'secure_attention' })) {
    markInputActivity()
    statusDetail.value = '已请求 Windows 显示安全选项'
  }
}

function handleMediaFrame(buffer: ArrayBuffer) {
  try {
    const frame = parseDesktopMediaFrame(buffer, audioNegotiated.value)
    if (frame.kind === 'audio') {
      handleAudioFrame(frame)
      return
    }
    queueVideoFrame(frame)
  } catch (error) {
    failProtocol(error instanceof Error ? error.message : '收到无法解析的远程桌面媒体帧')
  }
}

function queueVideoFrame(frame: DesktopVideoFrame) {
  pendingFrame = {
    ...frame,
    generation: socketGeneration,
    displayGeneration,
  }
  displayWidth.value = frame.width
  displayHeight.value = frame.height
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
      if (
        frame.generation !== socketGeneration
        || frame.displayGeneration !== displayGeneration
      ) {
        bitmap.close()
        continue
      }
      decodeTimes.push(performance.now() - startedAt)
      if (decodeTimes.length > 120) decodeTimes.shift()

      const previousBitmap = currentBitmap
      currentBitmap = bitmap
      latestSequence = frame.sequence
      drawCurrentFrame()
      if (!firstFrameRendered.value) {
        firstFrameRendered.value = true
        updateInteractiveState()
      }
      previousBitmap?.close()

      const now = performance.now()
      renderTimes.push(now)
      renderTimes = renderTimes.filter((value) => value >= now - 1000)
      renderedFps.value = renderTimes.length
    } catch {
      if (
        frame.generation !== socketGeneration
        || frame.displayGeneration !== displayGeneration
      ) continue
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

function changeDesktopQuality() {
  cacheDesktopQuality(desktopQuality.value)
  connect()
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
  const target = socket
  pauseRemoteAudio()
  connectionState.value = 'error'
  statusDetail.value = message
  releaseAllInputs()
  socketGeneration += 1
  socket = null
  target?.close(1002, 'invalid_desktop_message')
}

function desktopError(code: string, fallback: string) {
  const errors: Record<string, string> = {
    desktop_busy: '该实例已有管理员正在操作远程桌面',
    no_active_session: 'Windows 当前没有已登录的活动用户会话',
    multiple_active_sessions: 'Windows 存在多个活动用户会话，暂时无法自动选择',
    desktop_locked: 'Windows 桌面已锁定，解锁后可继续操作',
    secure_desktop: 'Windows 正在显示安全桌面，暂不支持远程操作',
    secure_attention_unavailable: 'Windows 或系统策略未允许发送 Ctrl+Alt+Del',
    secure_attention_denied: '当前远程会话未获准发送 Ctrl+Alt+Del',
    secure_attention_not_allowed: '当前远程会话未获准发送 Ctrl+Alt+Del',
    secure_attention_policy_denied: 'Windows 系统策略拒绝发送 Ctrl+Alt+Del',
    display_unavailable: 'Windows 当前没有可用显示设备，且虚拟显示器未能就绪',
    no_display_device: '未检测到物理显示器，虚拟显示器尚未就绪',
    no_display_output: '未检测到可捕获的 Windows 显示输出',
    virtual_device_reboot_required: 'Windows 需要重启后才能启用虚拟显示器',
    virtual_devices_disabled: '此 Agent 已禁用虚拟显示设备',
    driver_bundle_missing: '当前 Agent 未内置虚拟显示驱动',
    virtual_display_preparing: '正在准备虚拟显示器',
    virtual_display_driver_missing: '虚拟显示驱动未安装，无法创建远程画面',
    virtual_display_driver_unhealthy: '虚拟显示驱动运行异常，无法创建远程画面',
    virtual_display_reboot_required: '虚拟显示驱动需要重启 Windows 后生效',
    session_changed: 'Windows 交互会话已改变，请重新连接远程桌面',
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
    browser_heartbeat_timeout: '浏览器与服务端心跳超时，远程桌面会话已结束',
    agent_disconnected: 'Agent 已断开连接',
    client_closed: '远程桌面会话已关闭',
    browser_closed: '浏览器已关闭远程桌面连接',
    desktop_busy: '该实例已有管理员正在操作远程桌面',
    agent_draining: 'Agent 正在准备更新，暂不接受新的远程桌面会话',
    local_consent_denied: '远程计算机上的用户拒绝了本次远程桌面请求',
    local_consent_revoked: '远程计算机上的用户已终止本次远程桌面会话',
    control_rate_limited: '控制消息速率过高，远程桌面会话已终止',
    unsupported_platform: '该实例平台不支持远程桌面',
    helper_disconnected: 'Windows 桌面捕获进程已断开',
    helper_error: 'Windows 桌面捕获进程发生错误，请查看 Agent 日志',
    agent_data_error: 'Agent 远程桌面数据连接发生错误',
    data_channel_timeout: 'Agent 远程桌面数据通道连接超时',
    agent_error: 'Agent 处理远程桌面时发生错误，请查看 Agent 日志',
    frame_too_large: '桌面画面复杂度过高，JPEG 帧超过 2 MiB 限制',
    display_unavailable: 'Windows 当前没有可用显示设备，且虚拟显示器未能就绪',
    no_display_output: '未检测到可捕获的 Windows 显示输出',
    unattended_policy_rejected: 'Agent 未能确认无人值守安全策略，已拒绝本次连接',
    virtual_device_reboot_required: 'Windows 需要重启后才能启用虚拟显示器',
    virtual_devices_disabled: '此 Agent 已禁用虚拟显示设备',
    driver_bundle_missing: '当前 Agent 未内置虚拟显示驱动',
    virtual_display_reboot_required: '虚拟显示驱动需要重启 Windows 后生效',
    session_changed: 'Windows 交互会话已改变，请重新连接远程桌面',
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

function readDesktopQuality(): DesktopQuality {
  try {
    const value = window.localStorage.getItem(DESKTOP_QUALITY_STORAGE_KEY)
    if (isDesktopQuality(value)) return value
  } catch {
    // Storage may be unavailable in hardened browser contexts.
  }
  return 'balanced'
}

function cacheDesktopQuality(value: DesktopQuality) {
  try {
    window.localStorage.setItem(DESKTOP_QUALITY_STORAGE_KEY, value)
  } catch {
    // Keep the selection for this component lifetime when storage is unavailable.
  }
}

function isDesktopQuality(value: string | null): value is DesktopQuality {
  return value === 'low' || value === 'balanced' || value === 'high' || value === 'original'
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
              <span v-if="displaySourceLabel">{{ displaySourceLabel }}</span>
              <span v-if="sessionAccessMode === 'unattended'">无人值守</span>
              <span v-if="desktopKind !== 'default'">{{ desktopKind === 'secure' ? '安全桌面' : '系统桌面' }}</span>
            </p>
          </div>
        </div>

        <div class="remote-desktop-tools">
          <label class="desktop-quality-control" title="切换远程桌面画质">
            <Gauge :size="15" aria-hidden="true" />
            <select
              v-model="desktopQuality"
              aria-label="远程桌面画质"
              @change="changeDesktopQuality"
            >
              <option v-for="option in desktopQualityOptions" :key="option.value" :value="option.value">
                {{ option.label }}
              </option>
            </select>
          </label>
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
            v-if="instance.capabilities?.includes('remote_desktop_unattended_v1')"
            class="desktop-tool-button"
            type="button"
            :title="secureAttentionTitle"
            aria-label="发送 Ctrl+Alt+Del"
            :disabled="!canSendSecureAttention"
            @click="sendSecureAttention"
          >
            <Keyboard :size="15" />
            <span>Ctrl+Alt+Del</span>
          </button>
          <button
            :class="['desktop-tool-button', { active: audioEnabled }]"
            type="button"
            :title="audioTitle"
            :aria-label="audioTitle"
            :aria-pressed="audioEnabled"
            :disabled="!canToggleAudio"
            @click="toggleRemoteAudio"
          >
            <Volume2 v-if="audioEnabled" :size="15" />
            <VolumeX v-else :size="15" />
            <span>{{ audioEnabled ? '静音' : '声音' }}</span>
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
            <span>
              <LoaderCircle v-if="connectionState === 'connecting'" class="spin" :size="30" />
              <Monitor v-else :size="30" />
            </span>
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
        <div v-if="desktopNotice" class="remote-desktop-notice" role="alert">
          {{ desktopNotice }}
        </div>
      </div>
    </section>
  </div>
</template>
