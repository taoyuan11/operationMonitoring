const CHUNK_BYTES = 4 * 1024
const MAX_QUEUED_BYTES = 4 * 1024 * 1024
const MAX_SOCKET_BUFFER_BYTES = 64 * 1024
const FLUSH_INTERVAL_MS = 20

type InputSocket = Pick<WebSocket, 'readyState' | 'bufferedAmount' | 'send'>

export function createTerminalInput(getSocket: () => InputSocket | null) {
  let queue: Uint8Array[] = []
  let offset = 0
  let queuedBytes = 0
  let started = false
  let stopped = false
  let inputAck = false
  let nextId = 0
  let awaitingId: number | null = null
  let size: { cols: number; rows: number } | null = null
  let timer: ReturnType<typeof setTimeout> | null = null

  function schedule() {
    if (timer !== null || stopped || !started) return
    timer = setTimeout(() => {
      timer = null
      flush()
    }, FLUSH_INTERVAL_MS)
  }

  function flush() {
    if (stopped || !started) return
    const socket = getSocket()
    if (!socket || socket.readyState !== 1) return
    if (socket.bufferedAmount > MAX_SOCKET_BUFFER_BYTES) {
      schedule()
      return
    }
    try {
      if (size) {
        socket.send(JSON.stringify({ type: 'resize', ...size }))
        size = null
      }
      if (awaitingId !== null || !queue.length) return
      const bytes = queue[0]!.subarray(offset, offset + CHUNK_BYTES)
      const data = btoa(String.fromCharCode(...bytes))
      offset += bytes.length
      queuedBytes -= bytes.length
      if (offset === queue[0]!.length) {
        queue.shift()
        offset = 0
      }
      if (inputAck) {
        nextId = (nextId % 0xffff_ffff) + 1
        awaitingId = nextId
        socket.send(JSON.stringify({ type: 'input', data, input_id: nextId }))
      } else {
        // Older servers/agents do not acknowledge writes. Keep their frames
        // small and paced, without sending an unsupported acknowledgement ID.
        socket.send(JSON.stringify({ type: 'input', data }))
        if (queue.length) schedule()
      }
    } catch {
      // A socket can close between readyState and send. Never replay input:
      // it might already have executed; the socket handlers report the failure.
      stop()
    }
  }

  function stop() {
    stopped = true
    if (timer !== null) clearTimeout(timer)
    timer = null
    queue = []
    offset = 0
    queuedBytes = 0
    awaitingId = null
    size = null
  }

  return {
    start(supportsAck: boolean) {
      if (stopped || started) return
      started = true
      inputAck = supportsAck
      flush()
    },
    enqueue(value: string) {
      if (stopped || !value) return true
      // Reject the entire new paste before sending any of it when memory is
      // exhausted, instead of executing a truncated command or closing a shell.
      if (value.length > MAX_QUEUED_BYTES) return false
      const bytes = new TextEncoder().encode(value)
      if (queuedBytes + bytes.length > MAX_QUEUED_BYTES) return false
      queue.push(bytes)
      queuedBytes += bytes.length
      schedule()
      return true
    },
    acknowledge(inputId: number) {
      if (awaitingId !== inputId) return
      awaitingId = null
      flush()
    },
    resize(cols: number, rows: number) {
      if (stopped) return
      size = { cols, rows }
      schedule()
    },
    stop,
  }
}
