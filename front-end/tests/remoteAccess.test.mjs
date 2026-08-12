import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const source = await readFile(new URL('../src/api/remoteAccess.ts', import.meta.url), 'utf8')
const apiStubUrl = moduleUrl('export const api = async () => { throw new Error("not used") }')
const compiled = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText.replace(/from ['"]\.\/http['"]/, `from '${apiStubUrl}'`)
const { normalizeRemoteAccessStatus, remoteAccessCodeLabel } = await import(moduleUrl(compiled))

function moduleUrl(value) {
  return `data:text/javascript;base64,${Buffer.from(value).toString('base64')}`
}

test('normalizes reported virtual display and physical audio status', () => {
  assert.deepEqual(normalizeRemoteAccessStatus({
    protocol_supported: true,
    status_supported: true,
    online: true,
    access_mode: 'local_consent',
    fallback_mode: 'auto',
    display: {
      availability: 'ready',
      source: 'virtual',
      driver_state: 'active',
      driver_version: '1.2.3',
      code: null,
    },
    audio: {
      availability: 'degraded',
      source: 'physical',
      driver_state: 'standby',
      driver_version: null,
      code: 'windows_audio_disabled',
    },
    reboot_required: false,
    checked_at: 1_755_000_000,
  }), {
    protocol_supported: true,
    status_supported: true,
    online: true,
    access_mode: 'required',
    fallback_mode: 'auto',
    display: {
      availability: 'ready',
      source: 'virtual',
      driver_state: 'active',
      driver_version: '1.2.3',
      code: null,
    },
    audio: {
      availability: 'degraded',
      source: 'physical',
      driver_state: 'standby',
      driver_version: null,
      code: 'windows_audio_disabled',
    },
    reboot_required: false,
    checked_at: 1_755_000_000,
  })
})

test('degrades missing and future remote access fields without exposing raw errors', () => {
  const status = normalizeRemoteAccessStatus({
    protocol_supported: true,
    status_supported: false,
    access_mode: 'future_mode',
    display: null,
  })
  assert.equal(status.access_mode, 'unknown')
  assert.equal(status.fallback_mode, 'unknown')
  assert.equal(status.display.availability, 'unknown')
  assert.equal(status.audio.driver_state, 'unknown')
  assert.equal(status.checked_at, null)
  assert.equal(remoteAccessCodeLabel('windows_audio_disabled'), 'Windows Audio 服务未运行')
  assert.equal(remoteAccessCodeLabel('driver_bundle_missing'), '当前 Agent 未内置虚拟设备驱动')
  assert.equal(remoteAccessCodeLabel('raw_driver_failure_0xdead'), '远程访问设备状态异常')
})
