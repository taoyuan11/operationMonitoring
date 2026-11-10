import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const source = await readFile(new URL('../src/utils/controls.ts', import.meta.url), 'utf8')
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText
const { calendarDays, localDateTime, nextEnabledOption, stepNumber } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled).toString('base64')}`
)

test('number steps stay on the configured grid without decimal drift', () => {
  assert.equal(stepNumber(0.2, 1, 0.1, 0, 1), 0.3)
  assert.equal(stepNumber(0.25, 1, 0.1, 0, 1), 0.3)
  assert.equal(stepNumber(0.25, -1, 0.1, 0, 1), 0.2)
  assert.equal(stepNumber(0.3, -1, 0.1, 0, 1), 0.2)
  assert.equal(stepNumber(1, 1, 0.1, 0, 1), 1)
  assert.equal(stepNumber(0, -1, 0.1, 0, 1), 0)
  assert.equal(stepNumber(2, 1, 2, 1, 9), 3)
  assert.equal(stepNumber(null, 1, 1, -Infinity, Infinity), 1)
})

test('option navigation wraps around and skips unavailable choices', () => {
  const options = [
    { value: 'placeholder', label: '请选择', disabled: true },
    { value: 0, label: '零' },
    { value: 1, label: '一', disabled: true },
    { value: 2, label: '二' },
  ]
  assert.equal(nextEnabledOption(options, -1, 1), 1)
  assert.equal(nextEnabledOption(options, 1, 1), 3)
  assert.equal(nextEnabledOption(options, 3, 1), 1)
  assert.equal(nextEnabledOption(options, 1, -1), 3)
  assert.equal(nextEnabledOption([], 0, 1), -1)
  assert.equal(nextEnabledOption(options.map((option) => ({ ...option, disabled: true })), 0, 1), -1)
})

test('calendar has complete Monday-first weeks, including leap days and year boundaries', () => {
  const february = calendarDays(2028, 1)
  assert.equal(february.length, 42)
  assert.equal(february[0].getDay(), 1)
  assert.equal(february.filter((date) => date.getMonth() === 1).length, 29)
  const january = calendarDays(2027, 0)
  assert.equal(january[0].getFullYear(), 2026)
  assert.equal(january[0].getMonth(), 11)
  for (let index = 1; index < january.length; index++) {
    const next = new Date(january[index - 1])
    next.setDate(next.getDate() + 1)
    assert.equal(january[index].getTime(), next.getTime())
  }
})

test('date time values retain local wall-clock time rather than converting to UTC', () => {
  const date = new Date(2030, 4, 20, 13, 45)
  assert.equal(localDateTime(date), '2030-05-20T13:45')
  assert.equal(localDateTime(new Date(2030, 0, 2, 0, 5)), '2030-01-02T00:05')
})
