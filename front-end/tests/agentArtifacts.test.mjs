import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const source = await readFile(new URL('../src/utils/agentArtifacts.ts', import.meta.url), 'utf8')
const compiledSource = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(compiledSource).toString('base64')}`
const { inferArtifactTarget } = await import(moduleUrl)

const supportedArtifacts = [
  ['om-agent_0.1.20_linux_x86_64.bin', 'linux', 'x86_64'],
  ['om-agent_0.1.20_linux_x86_64-musl.bin', 'linux', 'x86_64-musl'],
  ['om-agent_0.1.20_linux_aarch64.bin', 'linux', 'aarch64'],
  ['om-agent_0.1.20_linux_arm.bin', 'linux', 'arm'],
  ['om-agent_0.1.20_linux_x86.bin', 'linux', 'x86'],
  ['om-agent_0.1.20_windows_x64.exe', 'windows', 'x64'],
  ['om-agent_0.1.20_windows_arm64.exe', 'windows', 'arm64'],
  ['om-agent_0.1.20_windows_x86.exe', 'windows', 'x86'],
  ['om-agent_0.1.20_macos_arm64.bin', 'macos', 'arm64'],
  ['om-agent_0.1.20_macos_x86_64.bin', 'macos', 'x86_64'],
]

test('recognizes every standalone artifact emitted by the build script', () => {
  for (const [fileName, os, nativeArch] of supportedArtifacts) {
    assert.deepEqual(inferArtifactTarget(fileName), {
      os,
      native_arch: nativeArch,
      inference: 'matched',
    })
  }
})

test('normalizes supported aliases and checksum file names', () => {
  const aliases = [
    ['OM-AGENT-1.2.3-LINUX-AMD64.BIN', 'linux', 'x86_64'],
    ['om-agent_1.2.3_linux_x86_64_musl.bin.sha256', 'linux', 'x86_64-musl'],
    ['om-agent_1.2.3-linux-armv7.bin', 'linux', 'arm'],
    ['om-agent_1.2.3_linux_i686.bin', 'linux', 'x86'],
    ['om-agent_1.2.3_windows_aarch64.exe', 'windows', 'arm64'],
    ['om-agent_1.2.3-macos-amd64.bin', 'macos', 'x86_64'],
  ]

  for (const [fileName, os, nativeArch] of aliases) {
    const target = inferArtifactTarget(fileName)
    assert.equal(target.os, os)
    assert.equal(target.native_arch, nativeArch)
  }
})

test('does not infer architecture from substrings or change bitness', () => {
  for (const fileName of [
    'om-agent_1.2.3_linux_charm.bin',
    'om-agent_1.2.3_linux_x86foo.bin',
    'om-agent_1.2.3_windows_arm.exe',
    'om-agent_1.2.3_macos_x86.bin',
  ]) {
    assert.equal(inferArtifactTarget(fileName).native_arch, '')
    assert.equal(inferArtifactTarget(fileName).inference, 'needs_architecture')
  }
})
