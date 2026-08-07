import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const rawSource = await readFile(new URL('../src/api/terminal.ts', import.meta.url), 'utf8')
const source = rawSource
  .replace("import { api } from './http'", 'const api = async () => undefined')
  .replace("import type { TerminalSessionStatus, TerminalShellListResponse } from '../types/domain'", '')
const compiledSource = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(compiledSource).toString('base64')}`
const {
  activeTerminalSessionCount,
  terminalShellsPath,
  terminalWebSocketPath,
  validCustomShellProgram,
} = await import(moduleUrl)

test('encodes instance identifiers and selected shell programs', () => {
  assert.equal(
    terminalShellsPath('edge/node 1'),
    '/api/admin/instances/edge%2Fnode%201/terminal/shells',
  )
  const selected = new URL(terminalWebSocketPath('edge/node 1', '/opt/custom shells/中文 shell'), 'http://localhost')
  assert.equal(selected.pathname, '/api/admin/instances/edge%2Fnode%201/terminal/ws')
  assert.equal(selected.searchParams.get('shell'), '/opt/custom shells/中文 shell')
  assert.equal(terminalWebSocketPath('node-1', null), '/api/admin/instances/node-1/terminal/ws')
})

test('accepts executable names and absolute-looking paths without accepting commands', () => {
  assert.equal(validCustomShellProgram('fish'), true)
  assert.equal(validCustomShellProgram('/opt/custom shells/fish', 'linux'), true)
  assert.equal(validCustomShellProgram('C:\\Program Files\\PowerShell\\7\\pwsh.exe', 'windows'), true)
  assert.equal(validCustomShellProgram('\\\\server\\shells\\pwsh.exe', 'windows'), true)
  assert.equal(validCustomShellProgram('relative/fish', 'linux'), false)
  assert.equal(validCustomShellProgram('relative\\pwsh.exe', 'windows'), false)
  assert.equal(validCustomShellProgram('/bin/zsh', 'windows'), false)
  assert.equal(validCustomShellProgram('C:\\shells\\pwsh.exe', 'linux'), false)
  assert.equal(validCustomShellProgram('fish --login'), false)
  assert.equal(validCustomShellProgram(' fish'), false)
  assert.equal(validCustomShellProgram('fish\nwhoami'), false)
  assert.equal(validCustomShellProgram('x'.repeat(1025)), false)
})

test('counts only opening and ready sessions for the target instance', () => {
  const sessions = [
    { instanceId: 'node-a', status: 'opening' },
    { instanceId: 'node-a', status: 'ready' },
    { instanceId: 'node-a', status: 'closed' },
    { instanceId: 'node-b', status: 'ready' },
  ]
  assert.equal(activeTerminalSessionCount(sessions, 'node-a'), 2)
  assert.equal(activeTerminalSessionCount(sessions, 'node-b'), 1)
})
