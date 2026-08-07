import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const source = await readFile(new URL('../src/utils/format.ts', import.meta.url), 'utf8')
const compiledSource = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(compiledSource).toString('base64')}`
const {
  formatDateTimeInput,
  formatDuration,
  formatExpirationRelative,
  parseDateTimeInput,
} = await import(moduleUrl)

test('round-trips local instance expiration input to Unix seconds', () => {
  const input = '2030-05-20T13:45'
  const timestamp = parseDateTimeInput(input)

  assert.equal(typeof timestamp, 'number')
  assert.equal(formatDateTimeInput(timestamp), input)
  assert.equal(parseDateTimeInput(''), null)
  assert.equal(parseDateTimeInput('not-a-date'), null)
})

test('formats runtime and expiration lifecycle states', () => {
  const now = 1_700_000_000

  assert.equal(formatDuration(0), '0秒')
  assert.equal(formatDuration(90), '1分')
  assert.equal(formatExpirationRelative(null, now), '长期有效')
  assert.equal(formatExpirationRelative(now + 2 * 86_400, now), '剩余 2天 0小时')
  assert.equal(formatExpirationRelative(now - 3_600, now), '已到期 1小时 0分')
})
