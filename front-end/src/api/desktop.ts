export const DESKTOP_AUDIO_CAPABILITY = 'remote_desktop_audio_v1'
export const DESKTOP_MEDIA_HEADER_BYTES = 32
export const DESKTOP_VIDEO_MAX_BYTES = 2 * 1024 * 1024
export const DESKTOP_OPUS_MAX_PACKET_BYTES = 1275
export const DESKTOP_OPUS_CHANNELS = 2
export const DESKTOP_OPUS_SAMPLE_RATE = 48_000
export const DESKTOP_OPUS_SAMPLES_PER_CHANNEL = 960
export const DESKTOP_AUDIO_DISCONTINUITY_FLAG = 0x01

export type DesktopQuality = 'low' | 'balanced' | 'high' | 'original'
export type DesktopAudioCodec = 'opus'
export type DesktopAudioServerState = 'off' | 'starting' | 'playing' | 'paused' | 'unavailable'
export type DesktopKind = 'default' | 'secure' | 'other'
export type DesktopContext = 'default' | 'winlogon' | 'other'
export type DesktopDisplaySource = 'physical' | 'virtual' | 'none' | 'unknown'
export type DesktopDisplayState = 'preparing' | 'ready' | 'unavailable' | 'unknown'
export type DesktopInteractionState =
  | 'preparing'
  | 'waiting_ready'
  | 'waiting_frame'
  | 'ready'
  | 'paused'
  | 'unavailable'

export type DesktopServerMessage =
  | { type: 'opening'; audio_codec?: string }
  | { type: 'consent_required' }
  | { type: 'ready' }
  | { type: 'display'; width: number; height: number }
  | {
    type: 'session_policy'
    access_mode: 'local_consent' | 'unattended'
    local_consent_required: boolean
    secure_desktop_control: boolean
    secure_attention_allowed: boolean
  }
  | {
    type: 'display_state'
    state: 'preparing' | 'ready' | 'unavailable'
    source: DesktopDisplaySource
    code?: string
  }
  | {
    type: 'desktop_state'
    desktop: DesktopKind
    context?: DesktopContext
    controllable?: boolean
  }
  | { type: 'notice'; code: string; message: string }
  | { type: 'audio_state'; state: DesktopAudioServerState; reason?: string }
  | { type: 'paused'; reason: string }
  | { type: 'closed'; reason: string }
  | { type: 'error'; code: string; message: string }

export type DesktopVideoFrame = {
  kind: 'video'
  sequence: number
  capturedAtMs: number
  width: number
  height: number
  jpeg: ArrayBuffer
}

export type DesktopAudioFrame = {
  kind: 'audio'
  sequence: number
  timestampUs: number
  sampleRate: number
  samplesPerChannel: number
  channels: number
  discontinuity: boolean
  opus: ArrayBuffer
}

export type DesktopMediaFrame = DesktopVideoFrame | DesktopAudioFrame
export type DesktopMediaKind = DesktopMediaFrame['kind'] | 'unknown'

export class DesktopMediaProtocolError extends Error {
  readonly mediaKind: DesktopMediaKind

  constructor(mediaKind: DesktopMediaKind, message: string) {
    super(message)
    this.name = 'DesktopMediaProtocolError'
    this.mediaKind = mediaKind
  }
}

export class DesktopControlProtocolError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'DesktopControlProtocolError'
  }
}

export function desktopWebSocketPath(
  instanceId: string,
  quality: DesktopQuality,
  audioCodec: DesktopAudioCodec | null,
) {
  const path = `/api/admin/instances/${encodeURIComponent(instanceId)}/desktop/ws`
  const query = new URLSearchParams({ quality })
  if (audioCodec) query.set('audio', audioCodec)
  return `${path}?${query.toString()}`
}

export function desktopWebSocketUrl(
  instanceId: string,
  quality: DesktopQuality,
  audioCodec: DesktopAudioCodec | null,
) {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${window.location.host}${desktopWebSocketPath(instanceId, quality, audioCodec)}`
}

export function parseDesktopMediaFrame(
  buffer: ArrayBuffer,
  audioNegotiated: boolean,
): DesktopMediaFrame {
  const bytes = new Uint8Array(buffer)
  if (bytes.byteLength < 4) {
    throw new DesktopMediaProtocolError('unknown', '收到大小异常的远程桌面媒体帧')
  }

  const magic = String.fromCharCode(bytes[0], bytes[1], bytes[2], bytes[3])
  if (magic === 'OMRD') return parseVideoFrame(buffer, bytes)
  if (magic === 'OMRA') {
    if (!audioNegotiated) {
      throw new DesktopMediaProtocolError('audio', '当前远程桌面会话未协商音频')
    }
    return parseAudioFrame(buffer)
  }
  throw new DesktopMediaProtocolError('unknown', '收到无效的远程桌面媒体帧标识')
}

export function parseDesktopServerMessage(payload: string): DesktopServerMessage {
  let value: unknown
  try {
    value = JSON.parse(payload)
  } catch {
    throw new DesktopControlProtocolError('收到无法解析的远程桌面消息')
  }
  const message = controlRecord(value)
  const type = requiredControlString(message, 'type')
  switch (type) {
    case 'opening':
      return { type, audio_codec: optionalControlString(message, 'audio_codec') }
    case 'consent_required':
    case 'ready':
      return { type }
    case 'display': {
      const width = controlDimension(message, 'width')
      const height = controlDimension(message, 'height')
      return { type, width, height }
    }
    case 'session_policy': {
      const accessMode = controlEnum(message, 'access_mode', ['local_consent', 'unattended'])
      const localConsentRequired = controlBoolean(message, 'local_consent_required')
      const secureDesktopControl = controlBoolean(message, 'secure_desktop_control')
      const secureAttentionAllowed = controlBoolean(message, 'secure_attention_allowed')
      if (
        localConsentRequired !== (accessMode === 'local_consent')
        || (secureAttentionAllowed && (accessMode !== 'unattended' || !secureDesktopControl))
      ) throw invalidControlMessage()
      return {
        type,
        access_mode: accessMode,
        local_consent_required: localConsentRequired,
        secure_desktop_control: secureDesktopControl,
        secure_attention_allowed: secureAttentionAllowed,
      }
    }
    case 'display_state': {
      const code = optionalControlString(message, 'code')
      if (code !== undefined && !/^[a-z0-9_]{1,64}$/.test(code)) throw invalidControlMessage()
      return {
        type,
        state: controlEnum(message, 'state', ['preparing', 'ready', 'unavailable']),
        source: controlEnum(message, 'source', ['physical', 'virtual', 'none', 'unknown']),
        code,
      }
    }
    case 'desktop_state': {
      const context = optionalControlString(message, 'context')
      const controllable = optionalControlBoolean(message, 'controllable')
      if (
        (context === undefined) !== (controllable === undefined)
        || (context !== undefined && !['default', 'winlogon', 'other'].includes(context))
      ) {
        throw invalidControlMessage()
      }
      return {
        type,
        desktop: controlEnum(message, 'desktop', ['default', 'secure', 'other']),
        context: context as DesktopContext | undefined,
        controllable,
      }
    }
    case 'notice':
      return {
        type,
        code: requiredControlString(message, 'code'),
        message: requiredControlString(message, 'message'),
      }
    case 'audio_state':
      return {
        type,
        state: controlEnum(message, 'state', ['off', 'starting', 'playing', 'paused', 'unavailable']),
        reason: optionalControlString(message, 'reason'),
      }
    case 'paused':
    case 'closed':
      return { type, reason: requiredControlString(message, 'reason') }
    case 'error':
      return {
        type,
        code: requiredControlString(message, 'code'),
        message: requiredControlString(message, 'message'),
      }
    default:
      throw new DesktopControlProtocolError('收到未知的远程桌面消息')
  }
}

export function desktopStateAllowsAudio(desktop: 'default' | 'secure' | 'other') {
  return desktop === 'default'
}

export function desktopMessageControllable(message: {
  desktop: DesktopKind
  controllable?: boolean
}) {
  return message.controllable ?? message.desktop === 'default'
}

export function resolveDesktopInteractionState(options: {
  displayState: DesktopDisplayState
  serverReady: boolean
  firstFrameRendered: boolean
  desktopControllable: boolean
}): DesktopInteractionState {
  if (options.displayState === 'preparing') return 'preparing'
  if (options.displayState === 'unavailable') return 'unavailable'
  if (!options.serverReady) return 'waiting_ready'
  if (!options.firstFrameRendered) return 'waiting_frame'
  return options.desktopControllable ? 'ready' : 'paused'
}

export function canSendDesktopSecureAttention(options: {
  accessMode: 'local_consent' | 'unattended'
  secureDesktopControl: boolean
  secureAttentionAllowed: boolean
  desktopControllable: boolean
  serverReady: boolean
  firstFrameRendered: boolean
}) {
  return options.accessMode === 'unattended'
    && options.secureDesktopControl
    && options.secureAttentionAllowed
    && options.desktopControllable
    && options.serverReady
    && options.firstFrameRendered
}

function controlRecord(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw invalidControlMessage()
  return value as Record<string, unknown>
}

function requiredControlString(record: Record<string, unknown>, key: string) {
  const value = record[key]
  if (typeof value !== 'string' || !value) throw invalidControlMessage()
  return value
}

function optionalControlString(record: Record<string, unknown>, key: string) {
  const value = record[key]
  if (value === undefined || value === null) return undefined
  if (typeof value !== 'string') throw invalidControlMessage()
  return value || undefined
}

function controlBoolean(record: Record<string, unknown>, key: string) {
  const value = record[key]
  if (typeof value !== 'boolean') throw invalidControlMessage()
  return value
}

function optionalControlBoolean(record: Record<string, unknown>, key: string) {
  const value = record[key]
  if (value === undefined) return undefined
  if (typeof value !== 'boolean') throw invalidControlMessage()
  return value
}

function controlDimension(record: Record<string, unknown>, key: string) {
  const value = record[key]
  if (!Number.isInteger(value) || (value as number) <= 0 || (value as number) > 16_384) {
    throw invalidControlMessage()
  }
  return value as number
}

function controlEnum<const T extends string>(
  record: Record<string, unknown>,
  key: string,
  values: readonly T[],
) {
  const value = record[key]
  if (typeof value !== 'string' || !values.includes(value as T)) throw invalidControlMessage()
  return value as T
}

function invalidControlMessage() {
  return new DesktopControlProtocolError('收到字段无效的远程桌面消息')
}

export function shouldReanchorAudio(options: {
  currentTime: number
  scheduledUntil: number
  decodeQueueSize: number
  previousSequence: number | null
  sequence: number
  discontinuity: boolean
}) {
  return options.discontinuity
    || options.decodeQueueSize >= 8
    || options.scheduledUntil > options.currentTime + 0.3
    || (options.previousSequence !== null && options.sequence !== options.previousSequence + 1)
}

function parseVideoFrame(buffer: ArrayBuffer, bytes: Uint8Array): DesktopVideoFrame {
  if (
    buffer.byteLength <= DESKTOP_MEDIA_HEADER_BYTES
    || buffer.byteLength > DESKTOP_VIDEO_MAX_BYTES
  ) {
    throw new DesktopMediaProtocolError('video', '收到大小异常的桌面画面')
  }

  const view = new DataView(buffer)
  if (view.getUint8(4) !== 1 || view.getUint8(5) !== 1 || view.getUint16(6, false) !== 0) {
    throw new DesktopMediaProtocolError('video', '当前浏览器不支持此桌面画面版本或编码')
  }

  const width = view.getUint32(24, false)
  const height = view.getUint32(28, false)
  if (!width || !height || width > 1920 || height > 1080) {
    throw new DesktopMediaProtocolError('video', '收到无效的桌面画面尺寸')
  }
  if (bytes[DESKTOP_MEDIA_HEADER_BYTES] !== 0xff || bytes[DESKTOP_MEDIA_HEADER_BYTES + 1] !== 0xd8) {
    throw new DesktopMediaProtocolError('video', '收到无效的桌面画面数据')
  }

  return {
    kind: 'video',
    sequence: safeHeaderInteger(view.getBigUint64(8, false), 'video'),
    capturedAtMs: safeHeaderInteger(view.getBigUint64(16, false), 'video'),
    width,
    height,
    jpeg: buffer.slice(DESKTOP_MEDIA_HEADER_BYTES),
  }
}

function parseAudioFrame(buffer: ArrayBuffer): DesktopAudioFrame {
  if (
    buffer.byteLength <= DESKTOP_MEDIA_HEADER_BYTES
    || buffer.byteLength > DESKTOP_MEDIA_HEADER_BYTES + DESKTOP_OPUS_MAX_PACKET_BYTES
  ) {
    throw new DesktopMediaProtocolError('audio', '收到大小异常的桌面音频帧')
  }

  const view = new DataView(buffer)
  const flags = view.getUint8(7)
  if (view.getUint8(4) !== 1 || view.getUint8(5) !== 1) {
    throw new DesktopMediaProtocolError('audio', '当前浏览器不支持此桌面音频版本或编码')
  }
  if (view.getUint8(6) !== DESKTOP_OPUS_CHANNELS || (flags & ~DESKTOP_AUDIO_DISCONTINUITY_FLAG) !== 0) {
    throw new DesktopMediaProtocolError('audio', '收到无效的桌面音频声道或标志')
  }

  const sampleRate = view.getUint32(24, false)
  const samplesPerChannel = view.getUint32(28, false)
  if (
    sampleRate !== DESKTOP_OPUS_SAMPLE_RATE
    || samplesPerChannel !== DESKTOP_OPUS_SAMPLES_PER_CHANNEL
  ) {
    throw new DesktopMediaProtocolError('audio', '收到无效的桌面音频采样参数')
  }

  return {
    kind: 'audio',
    sequence: safeHeaderInteger(view.getBigUint64(8, false), 'audio'),
    timestampUs: safeHeaderInteger(view.getBigUint64(16, false), 'audio'),
    sampleRate,
    samplesPerChannel,
    channels: DESKTOP_OPUS_CHANNELS,
    discontinuity: (flags & DESKTOP_AUDIO_DISCONTINUITY_FLAG) !== 0,
    opus: buffer.slice(DESKTOP_MEDIA_HEADER_BYTES),
  }
}

function safeHeaderInteger(value: bigint, mediaKind: DesktopMediaKind) {
  const number = Number(value)
  if (!Number.isSafeInteger(number)) {
    throw new DesktopMediaProtocolError(mediaKind, '收到超出浏览器安全范围的远程桌面时间戳或序号')
  }
  return number
}
