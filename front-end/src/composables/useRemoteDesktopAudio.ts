import { computed, ref } from 'vue'
import {
  DESKTOP_OPUS_CHANNELS,
  DESKTOP_OPUS_SAMPLE_RATE,
  type DesktopAudioServerState,
  type DesktopAudioCodec,
  type DesktopAudioFrame,
  shouldReanchorAudio,
} from '../api/desktop'

const OPUS_CONFIG: AudioDecoderConfig = {
  codec: 'opus',
  sampleRate: DESKTOP_OPUS_SAMPLE_RATE,
  numberOfChannels: DESKTOP_OPUS_CHANNELS,
}
const INITIAL_BUFFER_SECONDS = 0.1

type DesktopAudioState = 'checking' | 'unsupported' | 'muted' | 'starting' | 'playing' | 'paused' | 'unavailable' | 'error'

export function useRemoteDesktopAudio(options: {
  agentSupported: boolean
  sendControl: (enabled: boolean) => boolean
}) {
  const browserSupported = ref(false)
  const supportChecked = ref(false)
  const negotiated = ref(false)
  const connectionReady = ref(false)
  const enabled = ref(false)
  const state = ref<DesktopAudioState>(options.agentSupported ? 'checking' : 'unsupported')
  const reason = ref('')

  let supportPromise: Promise<boolean> | null = null
  let context: AudioContext | null = null
  let gain: GainNode | null = null
  let decoder: AudioDecoder | null = null
  let scheduledUntil = 0
  let previousSequence: number | null = null
  let serverStreaming = false
  let disposed = false
  let startGeneration = 0
  let playbackGeneration = 0
  const scheduledSources = new Set<AudioBufferSourceNode>()

  const requestedCodec = computed<DesktopAudioCodec | null>(() =>
    options.agentSupported && browserSupported.value ? 'opus' : null,
  )
  const muted = computed(() => !enabled.value)
  const canToggle = computed(() => enabled.value || (
    browserSupported.value && negotiated.value && connectionReady.value
  ))
  const title = computed(() => {
    if (!options.agentSupported) return '当前 Agent 不支持远程音频'
    if (!supportChecked.value) return '正在检查浏览器音频支持'
    if (!browserSupported.value) return '当前浏览器不支持 Opus 远程音频'
    if (enabled.value) {
      if (!negotiated.value || !connectionReady.value) return '远程音频已暂停，点击静音'
      if (state.value === 'unavailable') return audioUnavailableReason(reason.value)
      if (state.value === 'paused') return '远程音频已暂停，点击静音'
      if (state.value === 'starting') return '正在启用远程声音，点击静音'
      return '静音远程桌面'
    }
    if (!negotiated.value) return '当前远程桌面会话未启用音频'
    if (!connectionReady.value) return '远程桌面连接后可使用声音'
    if (state.value === 'error') return '远程音频播放失败，点击重试'
    return '播放远程桌面声音'
  })

  function prepare() {
    if (supportPromise) return supportPromise
    supportPromise = checkBrowserSupport()
    return supportPromise
  }

  async function checkBrowserSupport() {
    if (
      !options.agentSupported
      || typeof AudioDecoder === 'undefined'
      || typeof AudioContext === 'undefined'
      || typeof EncodedAudioChunk === 'undefined'
    ) {
      supportChecked.value = true
      state.value = 'unsupported'
      return false
    }
    try {
      const support = await AudioDecoder.isConfigSupported(OPUS_CONFIG)
      if (disposed) return false
      browserSupported.value = support.supported === true
    } catch {
      browserSupported.value = false
    }
    supportChecked.value = true
    state.value = browserSupported.value ? 'muted' : 'unsupported'
    return browserSupported.value
  }

  function handleOpening(codec: string | undefined) {
    resetConnection(true)
    negotiated.value = codec === 'opus' && browserSupported.value
    state.value = negotiated.value && enabled.value ? 'paused' : mutedState()
  }

  function handleConnectionReady() {
    if (connectionReady.value) return
    connectionReady.value = true
    if (!negotiated.value || !enabled.value) {
      state.value = mutedState()
      return
    }
    void restartEnabledAudio()
  }

  function handleConnectionPaused() {
    startGeneration += 1
    connectionReady.value = false
    serverStreaming = false
    resetPlayback()
    state.value = enabled.value ? 'paused' : mutedState()
  }

  function resetConnection(preserveIntent = true) {
    startGeneration += 1
    connectionReady.value = false
    negotiated.value = false
    serverStreaming = false
    if (!preserveIntent) enabled.value = false
    resetPlayback()
    reason.value = ''
    state.value = enabled.value ? 'paused' : mutedState()
  }

  async function toggle() {
    if (!canToggle.value) return
    if (enabled.value) {
      startGeneration += 1
      enabled.value = false
      serverStreaming = false
      if (negotiated.value) options.sendControl(false)
      resetPlayback()
      state.value = 'muted'
      reason.value = ''
      return
    }

    const generation = ++startGeneration
    enabled.value = true
    serverStreaming = false
    state.value = 'starting'
    reason.value = ''
    try {
      await ensurePlaybackReady()
      if (
        generation !== startGeneration
        || disposed
        || !enabled.value
        || !connectionReady.value
        || !negotiated.value
      ) return
      if (!options.sendControl(true)) {
        throw new Error('远程桌面连接已断开')
      }
    } catch (error) {
      if (generation !== startGeneration || disposed || !enabled.value) return
      fail(errorMessage(error, '浏览器无法启动远程音频'))
    }
  }

  function handleFrame(frame: DesktopAudioFrame) {
    if (
      !enabled.value
      || !negotiated.value
      || !connectionReady.value
      || !serverStreaming
    ) return
    try {
      const activeContext = context
      if (!activeContext || activeContext.state !== 'running') return
      if (shouldReanchorAudio({
        currentTime: activeContext.currentTime,
        scheduledUntil,
        decodeQueueSize: decoder?.decodeQueueSize ?? 0,
        previousSequence,
        sequence: frame.sequence,
        discontinuity: frame.discontinuity,
      })) {
        resetPlayback()
      }
      previousSequence = frame.sequence
      ensureDecoder().decode(new EncodedAudioChunk({
        type: 'key',
        timestamp: frame.timestampUs,
        duration: Math.round(frame.samplesPerChannel * 1_000_000 / frame.sampleRate),
        data: frame.opus,
      }))
    } catch (error) {
      fail(errorMessage(error, '浏览器无法解码远程音频'))
    }
  }

  function handleServerState(nextState: DesktopAudioServerState, nextReason = '') {
    reason.value = nextReason
    switch (nextState) {
      case 'off':
        serverStreaming = false
        resetPlayback()
        state.value = enabled.value ? 'starting' : 'muted'
        break
      case 'starting':
        serverStreaming = true
        state.value = enabled.value ? 'starting' : 'muted'
        break
      case 'playing':
        serverStreaming = true
        state.value = enabled.value ? 'playing' : 'muted'
        break
      case 'paused':
        serverStreaming = false
        resetPlayback()
        state.value = enabled.value ? 'paused' : 'muted'
        break
      case 'unavailable':
        serverStreaming = false
        resetPlayback()
        state.value = enabled.value ? 'unavailable' : 'muted'
        break
    }
  }

  function fail(message: string) {
    startGeneration += 1
    if (enabled.value && negotiated.value) options.sendControl(false)
    enabled.value = false
    serverStreaming = false
    reason.value = message
    resetPlayback()
    state.value = 'error'
  }

  function dispose() {
    disposed = true
    startGeneration += 1
    if (enabled.value && negotiated.value) options.sendControl(false)
    enabled.value = false
    connectionReady.value = false
    negotiated.value = false
    serverStreaming = false
    resetPlayback()
    const activeContext = context
    context = null
    gain = null
    if (activeContext && activeContext.state !== 'closed') void activeContext.close().catch(() => undefined)
  }

  async function restartEnabledAudio() {
    const generation = ++startGeneration
    try {
      serverStreaming = false
      await ensurePlaybackReady()
      if (
        generation !== startGeneration
        || disposed
        || !enabled.value
        || !connectionReady.value
        || !negotiated.value
      ) return
      if (!options.sendControl(true)) throw new Error('远程桌面连接已断开')
      state.value = 'starting'
    } catch (error) {
      if (generation !== startGeneration || disposed || !enabled.value) return
      fail(errorMessage(error, '浏览器无法恢复远程音频'))
    }
  }

  async function ensurePlaybackReady() {
    if (!context || context.state === 'closed') {
      context = new AudioContext({ latencyHint: 'interactive' })
      gain = context.createGain()
      gain.connect(context.destination)
    }
    gain!.gain.value = 1
    if (context.state !== 'running') await context.resume()
    if (context.state !== 'running') throw new Error('浏览器阻止了音频播放')
    ensureDecoder()
  }

  function ensureDecoder(): AudioDecoder {
    if (decoder?.state === 'configured') return decoder
    if (decoder?.state === 'unconfigured') {
      decoder.configure(OPUS_CONFIG)
      return decoder
    }
    const generation = playbackGeneration
    let nextDecoder: AudioDecoder
    nextDecoder = new AudioDecoder({
      output: (data) => playDecodedAudio(data, nextDecoder, generation),
      error: (error) => {
        if (
          disposed
          || generation !== playbackGeneration
          || decoder !== nextDecoder
        ) return
        fail(errorMessage(error, '浏览器无法解码远程音频'))
      },
    })
    decoder = nextDecoder
    try {
      nextDecoder.configure(OPUS_CONFIG)
    } catch (error) {
      decoder = null
      try {
        nextDecoder.close()
      } catch {
        // The decoder can already be closed after a configure failure.
      }
      throw error
    }
    return nextDecoder
  }

  function playDecodedAudio(
    data: AudioData,
    sourceDecoder: AudioDecoder,
    generation: number,
  ) {
    try {
      const activeContext = context
      const activeGain = gain
      if (
        disposed
        || generation !== playbackGeneration
        || sourceDecoder !== decoder
        || !enabled.value
        || !negotiated.value
        || !connectionReady.value
        || !serverStreaming
        || !activeContext
        || !activeGain
      ) return
      if (activeContext.state !== 'running') {
        fail('浏览器已暂停远程音频播放')
        return
      }
      if (
        data.numberOfChannels !== DESKTOP_OPUS_CHANNELS
        || data.sampleRate !== DESKTOP_OPUS_SAMPLE_RATE
      ) {
        fail('浏览器返回了无效的远程音频采样参数')
        return
      }

      if (scheduledUntil - activeContext.currentTime > 0.3) {
        stopScheduledSources()
        scheduledUntil = 0
      }
      const audioBuffer = activeContext.createBuffer(
        data.numberOfChannels,
        data.numberOfFrames,
        data.sampleRate,
      )
      for (let channel = 0; channel < data.numberOfChannels; channel += 1) {
        const samples = new Float32Array(data.numberOfFrames)
        data.copyTo(samples, { planeIndex: channel, format: 'f32-planar' })
        audioBuffer.copyToChannel(samples, channel)
      }

      const source = activeContext.createBufferSource()
      source.buffer = audioBuffer
      source.connect(activeGain)
      source.onended = () => {
        scheduledSources.delete(source)
        source.disconnect()
      }
      const startAt = scheduledUntil > activeContext.currentTime
        ? scheduledUntil
        : activeContext.currentTime + INITIAL_BUFFER_SECONDS
      scheduledUntil = startAt + audioBuffer.duration
      scheduledSources.add(source)
      source.start(startAt)
    } catch (error) {
      fail(errorMessage(error, '浏览器无法播放远程音频'))
    } finally {
      data.close()
    }
  }

  function resetPlayback() {
    playbackGeneration += 1
    previousSequence = null
    scheduledUntil = 0
    stopScheduledSources()
    const activeDecoder = decoder
    decoder = null
    if (!activeDecoder || activeDecoder.state === 'closed') return
    try {
      activeDecoder.close()
    } catch {
      // The decoder can already be closed by its error callback.
    }
  }

  function stopScheduledSources() {
    for (const source of scheduledSources) {
      source.onended = null
      try {
        source.stop()
      } catch {
        // A source that already ended cannot be stopped again.
      }
      source.disconnect()
    }
    scheduledSources.clear()
  }

  function mutedState(): DesktopAudioState {
    if (!options.agentSupported || (supportChecked.value && !browserSupported.value)) return 'unsupported'
    return supportChecked.value ? 'muted' : 'checking'
  }

  return {
    browserSupported,
    canToggle,
    enabled,
    muted,
    negotiated,
    requestedCodec,
    state,
    title,
    dispose,
    fail,
    handleConnectionPaused,
    handleConnectionReady,
    handleFrame,
    handleOpening,
    handleServerState,
    prepare,
    resetConnection,
    toggle,
  }
}

function audioUnavailableReason(reason: string) {
  const reasons: Record<string, string> = {
    secure_desktop: '安全桌面期间远程音频已暂停',
    no_output_device: '远程计算机没有可用的音频输出设备',
    audio_service_unavailable: '远程计算机的 Windows 音频服务不可用',
    device_invalidated: '远程计算机的音频输出设备已变更',
    user_token_unavailable: '无法访问远程用户的音频会话',
    capture_failed: '远程计算机无法捕获系统声音',
    encoder_failed: '远程计算机无法编码系统声音',
  }
  return reasons[reason] || '远程音频暂时不可用，点击静音'
}

function errorMessage(error: unknown, fallback: string) {
  return error instanceof Error && error.message ? error.message : fallback
}
