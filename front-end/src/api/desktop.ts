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

export function desktopStateAllowsAudio(desktop: 'default' | 'secure' | 'other') {
  return desktop === 'default'
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
