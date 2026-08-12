import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const source = await readFile(new URL('../src/api/desktop.ts', import.meta.url), 'utf8')
const compiledSource = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(compiledSource).toString('base64')}`
const {
  canSendDesktopSecureAttention,
  DESKTOP_AUDIO_DISCONTINUITY_FLAG,
  DesktopControlProtocolError,
  DESKTOP_MEDIA_HEADER_BYTES,
  DESKTOP_OPUS_MAX_PACKET_BYTES,
  DesktopMediaProtocolError,
  desktopMessageControllable,
  desktopStateAllowsAudio,
  desktopWebSocketPath,
  parseDesktopMediaFrame,
  parseDesktopServerMessage,
  resolveDesktopInteractionState,
  shouldReanchorAudio,
} = await import(moduleUrl)

const audioComposableSource = await readFile(
  new URL('../src/composables/useRemoteDesktopAudio.ts', import.meta.url),
  'utf8',
)
const vueStubUrl = sourceModuleUrl(`
  export const ref = (value) => ({ value })
  export const computed = (getter) => ({ get value() { return getter() } })
`)
const desktopStubUrl = sourceModuleUrl(`
  export const DESKTOP_OPUS_CHANNELS = 2
  export const DESKTOP_OPUS_SAMPLE_RATE = 48000
  export const shouldReanchorAudio = () => false
`)
const audioComposableModuleUrl = sourceModuleUrl(
  ts.transpileModule(audioComposableSource, {
    compilerOptions: {
      module: ts.ModuleKind.ESNext,
      target: ts.ScriptTarget.ES2022,
    },
  }).outputText
    .replace(/from ['"]vue['"]/, `from '${vueStubUrl}'`)
    .replace(/from ['"]\.\.\/api\/desktop['"]/, `from '${desktopStubUrl}'`),
)
const { useRemoteDesktopAudio } = await import(audioComposableModuleUrl)

function sourceModuleUrl(moduleSource) {
  return `data:text/javascript;base64,${Buffer.from(moduleSource).toString('base64')}`
}

function installAudioMocks(resume, harness = {}) {
  const original = {
    AudioContext: globalThis.AudioContext,
    AudioDecoder: globalThis.AudioDecoder,
    EncodedAudioChunk: globalThis.EncodedAudioChunk,
  }

  class MockAudioContext {
    state = 'suspended'
    currentTime = 0
    destination = {}

    createGain() {
      return { gain: { value: 0 }, connect() {} }
    }

    createBuffer(_channels, frames, sampleRate) {
      return {
        duration: frames / sampleRate,
        copyToChannel() {},
      }
    }

    createBufferSource() {
      const source = {
        buffer: null,
        onended: null,
        connect() {},
        disconnect() {},
        start(at) {
          harness.starts?.push(at)
        },
        stop() {},
      }
      harness.sources?.push(source)
      return source
    }

    async resume() {
      await resume()
      this.state = 'running'
    }

    async close() {
      this.state = 'closed'
    }
  }

  class MockAudioDecoder {
    static async isConfigSupported() {
      return { supported: true }
    }

    state = 'unconfigured'
    decodeQueueSize = 0
    chunks = []

    constructor(init) {
      this.init = init
      harness.decoders?.push(this)
    }

    configure() {
      this.state = 'configured'
    }

    reset() {
      this.state = 'unconfigured'
    }

    decode(chunk) {
      this.chunks.push(chunk)
    }

    close() {
      this.state = 'closed'
    }
  }

  globalThis.AudioContext = MockAudioContext
  globalThis.AudioDecoder = MockAudioDecoder
  globalThis.EncodedAudioChunk = class {
    constructor(init) {
      Object.assign(this, init)
    }
  }

  return () => {
    restoreGlobal('AudioContext', original.AudioContext)
    restoreGlobal('AudioDecoder', original.AudioDecoder)
    restoreGlobal('EncodedAudioChunk', original.EncodedAudioChunk)
  }
}

function decodedAudioData() {
  return {
    numberOfChannels: 2,
    numberOfFrames: 960,
    sampleRate: 48_000,
    closed: false,
    copyTo() {},
    close() {
      this.closed = true
    },
  }
}

function parsedAudioFrame(sequence = 1) {
  return {
    kind: 'audio',
    sequence,
    timestampUs: sequence * 20_000,
    sampleRate: 48_000,
    samplesPerChannel: 960,
    channels: 2,
    discontinuity: false,
    opus: Uint8Array.from([0xf8, 0xff, 0xfe]).buffer,
  }
}

function restoreGlobal(name, value) {
  if (value === undefined) delete globalThis[name]
  else globalThis[name] = value
}

async function readyAudio(sendControl) {
  const audio = useRemoteDesktopAudio({ agentSupported: true, sendControl })
  assert.equal(await audio.prepare(), true)
  audio.handleOpening('opus')
  audio.handleConnectionReady()
  return audio
}

function videoFrame() {
  const buffer = new ArrayBuffer(DESKTOP_MEDIA_HEADER_BYTES + 2)
  const bytes = new Uint8Array(buffer)
  const view = new DataView(buffer)
  bytes.set(Buffer.from('OMRD'))
  view.setUint8(4, 1)
  view.setUint8(5, 1)
  view.setBigUint64(8, 7n, false)
  view.setBigUint64(16, 1_725_000_000_123n, false)
  view.setUint32(24, 1920, false)
  view.setUint32(28, 1080, false)
  bytes.set([0xff, 0xd8], DESKTOP_MEDIA_HEADER_BYTES)
  return buffer
}

function audioFrame(flags = 0) {
  const buffer = new ArrayBuffer(DESKTOP_MEDIA_HEADER_BYTES + 3)
  const bytes = new Uint8Array(buffer)
  const view = new DataView(buffer)
  bytes.set(Buffer.from('OMRA'))
  view.setUint8(4, 1)
  view.setUint8(5, 1)
  view.setUint8(6, 2)
  view.setUint8(7, flags)
  view.setBigUint64(8, 0x0102_0304_0506n, false)
  view.setBigUint64(16, 1_725_000_000_123_456n, false)
  view.setUint32(24, 48_000, false)
  view.setUint32(28, 960, false)
  bytes.set([0xf8, 0xff, 0xfe], DESKTOP_MEDIA_HEADER_BYTES)
  return buffer
}

test('builds desktop websocket paths with explicit optional audio negotiation', () => {
  const audio = new URL(
    desktopWebSocketPath('edge/node 1', 'high', 'opus'),
    'http://localhost',
  )
  assert.equal(audio.pathname, '/api/admin/instances/edge%2Fnode%201/desktop/ws')
  assert.deepEqual(Object.fromEntries(audio.searchParams), { quality: 'high', audio: 'opus' })
  assert.equal(
    desktopWebSocketPath('node-1', 'balanced', null),
    '/api/admin/instances/node-1/desktop/ws?quality=balanced',
  )
})

test('parses the existing OMRD JPEG envelope without changing its big-endian fields', () => {
  const frame = parseDesktopMediaFrame(videoFrame(), false)
  assert.deepEqual(frame, {
    kind: 'video',
    sequence: 7,
    capturedAtMs: 1_725_000_000_123,
    width: 1920,
    height: 1080,
    jpeg: new Uint8Array([0xff, 0xd8]).buffer,
  })
})

test('parses strict OMRA Opus metadata and the discontinuity flag', () => {
  const continuous = parseDesktopMediaFrame(audioFrame(), true)
  assert.equal(continuous.kind, 'audio')
  assert.equal(continuous.sequence, Number(0x0102_0304_0506n))
  assert.equal(continuous.timestampUs, 1_725_000_000_123_456)
  assert.equal(continuous.sampleRate, 48_000)
  assert.equal(continuous.samplesPerChannel, 960)
  assert.equal(continuous.channels, 2)
  assert.equal(continuous.discontinuity, false)
  assert.deepEqual(new Uint8Array(continuous.opus), new Uint8Array([0xf8, 0xff, 0xfe]))

  const discontinuity = parseDesktopMediaFrame(audioFrame(DESKTOP_AUDIO_DISCONTINUITY_FLAG), true)
  assert.equal(discontinuity.kind, 'audio')
  assert.equal(discontinuity.discontinuity, true)
})

test('classifies unsupported or malformed OMRA headers as media protocol errors', () => {
  for (const [index, value] of [[4, 2], [5, 2], [6, 1], [7, 2]]) {
    const invalid = audioFrame()
    new DataView(invalid).setUint8(index, value)
    assert.throws(
      () => parseDesktopMediaFrame(invalid, true),
      (error) => error instanceof DesktopMediaProtocolError && error.mediaKind === 'audio',
    )
  }

  const invalidRate = audioFrame()
  new DataView(invalidRate).setUint32(24, 44_100, false)
  assert.throws(() => parseDesktopMediaFrame(invalidRate, true), /采样参数/)

  const invalidSamples = audioFrame()
  new DataView(invalidSamples).setUint32(28, 480, false)
  assert.throws(() => parseDesktopMediaFrame(invalidSamples, true), /采样参数/)

  const empty = audioFrame().slice(0, DESKTOP_MEDIA_HEADER_BYTES)
  assert.throws(() => parseDesktopMediaFrame(empty, true), /音频帧/)

  const oversized = new ArrayBuffer(DESKTOP_MEDIA_HEADER_BYTES + DESKTOP_OPUS_MAX_PACKET_BYTES + 1)
  new Uint8Array(oversized).set(new Uint8Array(audioFrame()).subarray(0, DESKTOP_MEDIA_HEADER_BYTES))
  assert.throws(() => parseDesktopMediaFrame(oversized, true), /音频帧/)
})

test('rejects valid audio frames unless opening negotiated Opus', () => {
  assert.throws(
    () => parseDesktopMediaFrame(audioFrame(), false),
    (error) => error instanceof DesktopMediaProtocolError
      && error.mediaKind === 'audio'
      && /未协商音频/.test(error.message),
  )
})

test('keeps unknown media distinguishable from identified audio protocol failures', () => {
  const invalid = Uint8Array.from(Buffer.from('NOPE')).buffer
  assert.throws(
    () => parseDesktopMediaFrame(invalid, false),
    (error) => error instanceof DesktopMediaProtocolError && error.mediaKind === 'unknown',
  )
})

test('restores audio readiness only on the default Windows desktop', () => {
  assert.equal(desktopStateAllowsAudio('default'), true)
  assert.equal(desktopStateAllowsAudio('secure'), false)
  assert.equal(desktopStateAllowsAudio('other'), false)
})

test('strictly parses the extended desktop policy and device status messages', () => {
  assert.deepEqual(parseDesktopServerMessage(JSON.stringify({
    type: 'session_policy',
    access_mode: 'unattended',
    local_consent_required: false,
    secure_desktop_control: true,
    secure_attention_allowed: true,
  })), {
    type: 'session_policy',
    access_mode: 'unattended',
    local_consent_required: false,
    secure_desktop_control: true,
    secure_attention_allowed: true,
  })
  assert.deepEqual(parseDesktopServerMessage(JSON.stringify({
    type: 'display_state',
    state: 'preparing',
    source: 'virtual',
    code: 'virtual_display_preparing',
  })), {
    type: 'display_state',
    state: 'preparing',
    source: 'virtual',
    code: 'virtual_display_preparing',
  })
  assert.deepEqual(parseDesktopServerMessage(JSON.stringify({
    type: 'desktop_state',
    desktop: 'secure',
    context: 'winlogon',
    controllable: true,
  })), {
    type: 'desktop_state',
    desktop: 'secure',
    context: 'winlogon',
    controllable: true,
  })
})

test('rejects inconsistent unattended policy and partial desktop context messages', () => {
  for (const message of [
    {
      type: 'session_policy',
      access_mode: 'unattended',
      local_consent_required: true,
      secure_desktop_control: true,
      secure_attention_allowed: true,
    },
    {
      type: 'session_policy',
      access_mode: 'local_consent',
      local_consent_required: true,
      secure_desktop_control: false,
      secure_attention_allowed: true,
    },
    { type: 'display_state', state: 'ready', source: 'virtual', code: 'Bad Code' },
    { type: 'desktop_state', desktop: 'secure', context: 'winlogon' },
  ]) {
    assert.throws(
      () => parseDesktopServerMessage(JSON.stringify(message)),
      (error) => error instanceof DesktopControlProtocolError,
    )
  }
})

test('keeps legacy desktop state default-only and gates readiness on ready plus first frame', () => {
  const legacyDefault = parseDesktopServerMessage('{"type":"desktop_state","desktop":"default"}')
  const legacySecure = parseDesktopServerMessage('{"type":"desktop_state","desktop":"secure"}')
  assert.equal(desktopMessageControllable(legacyDefault), true)
  assert.equal(desktopMessageControllable(legacySecure), false)

  const base = {
    displayState: 'ready',
    serverReady: false,
    firstFrameRendered: false,
    desktopControllable: true,
  }
  assert.equal(resolveDesktopInteractionState(base), 'waiting_ready')
  assert.equal(resolveDesktopInteractionState({ ...base, serverReady: true }), 'waiting_frame')
  assert.equal(resolveDesktopInteractionState({
    ...base,
    serverReady: true,
    firstFrameRendered: true,
  }), 'ready')
  assert.equal(resolveDesktopInteractionState({
    ...base,
    serverReady: true,
    firstFrameRendered: true,
    desktopControllable: false,
  }), 'paused')
  assert.equal(resolveDesktopInteractionState({ ...base, displayState: 'preparing' }), 'preparing')
  assert.equal(resolveDesktopInteractionState({ ...base, displayState: 'unavailable' }), 'unavailable')
})

test('allows Ctrl+Alt+Del only for a ready controllable unattended session', () => {
  const allowed = {
    accessMode: 'unattended',
    secureDesktopControl: true,
    secureAttentionAllowed: true,
    desktopControllable: true,
    serverReady: true,
    firstFrameRendered: true,
  }
  assert.equal(canSendDesktopSecureAttention(allowed), true)
  for (const key of Object.keys(allowed)) {
    if (key === 'accessMode') continue
    assert.equal(canSendDesktopSecureAttention({ ...allowed, [key]: false }), false)
  }
  assert.equal(canSendDesktopSecureAttention({ ...allowed, accessMode: 'local_consent' }), false)
})

test('reanchors audio only at the bounded latency and continuity limits', () => {
  const stable = {
    currentTime: 10,
    scheduledUntil: 10.3,
    decodeQueueSize: 7,
    previousSequence: 40,
    sequence: 41,
    discontinuity: false,
  }
  assert.equal(shouldReanchorAudio(stable), false)
  assert.equal(shouldReanchorAudio({ ...stable, discontinuity: true }), true)
  assert.equal(shouldReanchorAudio({ ...stable, decodeQueueSize: 8 }), true)
  assert.equal(shouldReanchorAudio({ ...stable, scheduledUntil: 10.301 }), true)
  assert.equal(shouldReanchorAudio({ ...stable, sequence: 42 }), true)
  assert.equal(shouldReanchorAudio({ ...stable, previousSequence: null, sequence: 999 }), false)
})

test('does not let a stale audio resume send enable after an immediate mute', async () => {
  let resolveResume
  const resumeFinished = new Promise((resolve) => { resolveResume = resolve })
  const restore = installAudioMocks(() => resumeFinished)
  try {
    const controls = []
    const audio = await readyAudio((enabled) => {
      controls.push(enabled)
      return true
    })

    const enabling = audio.toggle()
    await Promise.resolve()
    assert.equal(audio.enabled.value, true)
    assert.equal(audio.canToggle.value, true)

    await audio.toggle()
    assert.equal(audio.enabled.value, false)
    assert.deepEqual(controls, [false])

    resolveResume()
    await enabling
    assert.deepEqual(controls, [false])
  } finally {
    restore()
  }
})

test('keeps the mute control available while an enabled session is paused', async () => {
  const restore = installAudioMocks(async () => undefined)
  try {
    const controls = []
    const audio = await readyAudio((enabled) => {
      controls.push(enabled)
      return true
    })

    await audio.toggle()
    assert.deepEqual(controls, [true])
    audio.handleConnectionPaused()
    assert.equal(audio.enabled.value, true)
    assert.equal(audio.canToggle.value, true)

    await audio.toggle()
    assert.equal(audio.enabled.value, false)
    assert.deepEqual(controls, [true, false])
  } finally {
    restore()
  }
})

test('restores enabled audio only once for duplicate ready messages', async () => {
  const restore = installAudioMocks(async () => undefined)
  try {
    const controls = []
    const audio = await readyAudio((enabled) => {
      controls.push(enabled)
      return true
    })

    await audio.toggle()
    audio.handleConnectionPaused()
    audio.handleConnectionReady()
    audio.handleConnectionReady()
    await new Promise((resolve) => setImmediate(resolve))

    assert.deepEqual(controls, [true, true])
  } finally {
    restore()
  }
})

test('does not send audio control before the replacement connection negotiates Opus', async () => {
  const restore = installAudioMocks(async () => undefined)
  try {
    const controls = []
    const audio = await readyAudio((enabled) => {
      controls.push(enabled)
      return true
    })

    await audio.toggle()
    assert.deepEqual(controls, [true])

    audio.resetConnection(true)
    assert.equal(audio.enabled.value, true)
    await audio.toggle()

    assert.equal(audio.enabled.value, false)
    assert.deepEqual(controls, [true])
  } finally {
    restore()
  }
})

test('drops stale decoder callbacks after reconnect without disabling restored audio', async () => {
  const harness = { decoders: [], sources: [], starts: [] }
  const restore = installAudioMocks(async () => undefined, harness)
  try {
    const controls = []
    const audio = await readyAudio((enabled) => {
      controls.push(enabled)
      return true
    })

    await audio.toggle()
    audio.handleServerState('playing')
    audio.handleFrame(parsedAudioFrame())
    const staleDecoder = harness.decoders[0]
    assert.equal(staleDecoder.chunks.length, 1)

    audio.resetConnection(true)
    audio.handleOpening('opus')
    audio.handleConnectionReady()
    await new Promise((resolve) => setImmediate(resolve))
    audio.handleServerState('playing')

    assert.deepEqual(controls, [true, true])
    assert.equal(harness.decoders.length, 2)
    assert.equal(staleDecoder.state, 'closed')

    const staleData = decodedAudioData()
    staleDecoder.init.output(staleData)
    staleDecoder.init.error(new Error('stale decoder failure'))
    assert.equal(staleData.closed, true)
    assert.equal(audio.enabled.value, true)
    assert.equal(audio.state.value, 'playing')
    assert.deepEqual(harness.starts, [])

    const activeDecoder = harness.decoders[1]
    audio.handleFrame(parsedAudioFrame(2))
    const activeData = decodedAudioData()
    activeDecoder.init.output(activeData)
    assert.equal(activeData.closed, true)
    assert.deepEqual(harness.starts, [0.1])
  } finally {
    restore()
  }
})
