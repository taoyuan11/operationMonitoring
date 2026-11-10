import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const source = await readFile(new URL('../src/utils/terminalInput.ts', import.meta.url), 'utf8')
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText
const { createTerminalInput } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString('base64')}`)

function connection(t) {
  t.mock.timers.enable({ apis: ['setTimeout'] })
  const messages = []
  const socket = { readyState: 1, bufferedAmount: 0, send: (value) => messages.push(JSON.parse(value)) }
  const input = createTerminalInput(() => socket)
  t.after(() => input.stop())
  return { input, messages, socket }
}

test('large multilingual paste is byte-exact and waits for each PTY acknowledgement', (t) => {
  const { input, messages } = connection(t)
  const text = '中文🙂\r\n'.repeat(40_000)
  assert.equal(input.enqueue(text), true)
  t.mock.timers.tick(100)
  assert.equal(messages.length, 0, 'wait for ready before sending input')
  input.start(true)
  assert.equal(messages.length, 1)
  t.mock.timers.tick(10_000)
  assert.equal(messages.length, 1, 'a slow shell must not receive more input')
  input.acknowledge(999)
  assert.equal(messages.length, 1)
  const bytes = []
  for (let index = 0; index < messages.length; index++) {
    const message = messages[index]
    assert.equal(message.type, 'input')
    assert.ok(message.data.length <= 64 * 1024)
    bytes.push(Buffer.from(message.data, 'base64'))
    input.acknowledge(message.input_id)
    input.acknowledge(message.input_id) // A duplicate must not release the next chunk.
    assert.ok(messages.length <= index + 2)
  }
  assert.equal(Buffer.concat(bytes).toString(), text)
})

test('legacy agents receive small paced frames without acknowledgement IDs', (t) => {
  const { input, messages } = connection(t)
  input.start(false)
  input.enqueue('a'.repeat(12_000))
  assert.equal(messages.length, 0)
  t.mock.timers.tick(20)
  assert.equal(messages.length, 1)
  t.mock.timers.tick(20)
  assert.equal(messages.length, 2)
  t.mock.timers.tick(20)
  assert.equal(messages.length, 3)
  assert.ok(messages.every((message) => message.input_id === undefined))
  assert.equal(Buffer.concat(messages.map((message) => Buffer.from(message.data, 'base64'))).length, 12_000)
})

test('socket backpressure pauses input and resize bursts preserve only the latest size', (t) => {
  const { input, messages, socket } = connection(t)
  input.start(true)
  socket.bufferedAmount = 128 * 1024
  input.enqueue('echo hello\r')
  for (let cols = 80; cols <= 160; cols++) input.resize(cols, 40)
  t.mock.timers.tick(20)
  assert.equal(messages.length, 0)
  socket.bufferedAmount = 0
  t.mock.timers.tick(20)
  assert.deepEqual(messages[0], { type: 'resize', cols: 160, rows: 40 })
  assert.equal(messages[1].type, 'input')
  input.resize(180, 50)
  t.mock.timers.tick(20)
  assert.deepEqual(messages[2], { type: 'resize', cols: 180, rows: 50 })
})

test('oversized new input is rejected atomically without closing or corrupting queued commands', (t) => {
  const { input, messages, socket } = connection(t)
  input.start(true)
  input.enqueue('echo safe\r')
  assert.equal(input.enqueue('中'.repeat(2 * 1024 * 1024)), false)
  t.mock.timers.tick(20)
  assert.equal(messages.length, 1)
  assert.equal(Buffer.from(messages[0].data, 'base64').toString(), 'echo safe\r')
  assert.equal(socket.readyState, 1)
})

test('closing a session cancels queued input, timers, and late acknowledgements', (t) => {
  const { input, messages } = connection(t)
  input.start(true)
  input.enqueue('a'.repeat(12_000))
  t.mock.timers.tick(20)
  assert.equal(messages.length, 1)
  input.stop()
  input.acknowledge(messages[0].input_id)
  input.enqueue('should not execute\r')
  input.resize(100, 50)
  t.mock.timers.tick(10_000)
  assert.equal(messages.length, 1)
})
