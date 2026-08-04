import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import test from 'node:test'
import ts from 'typescript'

const rawSource = await readFile(new URL('../src/api/alerts.ts', import.meta.url), 'utf8')
const source = rawSource.replace(
  "import { api } from './http'",
  "const api = async () => { throw new Error('API calls are not used by query tests') }",
)
const compiledSource = ts.transpileModule(source, {
  compilerOptions: {
    module: ts.ModuleKind.ESNext,
    target: ts.ScriptTarget.ES2022,
  },
}).outputText
const moduleUrl = `data:text/javascript;base64,${Buffer.from(compiledSource).toString('base64')}`
const { alertDeliveryQueryPath, alertEventQueryPath } = await import(moduleUrl)

test('serializes all event filters with stable backend field names', () => {
  const path = alertEventQueryPath({
    page: 3,
    page_size: 25,
    status: 'acknowledged',
    severity: 'critical',
    metric: 'latency_ms',
    instance_id: 'node/edge 1',
    suppressed: 'false',
    from: 1_700_000_000,
    to: 1_700_003_600,
    search: '  核心 节点  ',
  })
  const url = new URL(path, 'http://localhost')

  assert.equal(url.pathname, '/api/admin/alerts/events')
  assert.deepEqual(Object.fromEntries(url.searchParams), {
    page: '3',
    page_size: '25',
    status: 'acknowledged',
    severity: 'critical',
    metric: 'latency_ms',
    instance_id: 'node/edge 1',
    suppressed: 'false',
    search: '核心 节点',
    from: '1700000000',
    to: '1700003600',
  })
})

test('omits unset event filters while preserving pagination', () => {
  assert.equal(
    alertEventQueryPath({
      page: 1,
      page_size: 50,
      status: '',
      severity: '',
      metric: '',
      instance_id: '',
      suppressed: '',
      from: null,
      to: null,
      search: '   ',
    }),
    '/api/admin/alerts/events?page=1&page_size=50',
  )
})

test('serializes delivery filters and omits blank event identifiers', () => {
  const filtered = new URL(alertDeliveryQueryPath({
    page: 2,
    page_size: 100,
    status: 'failed',
    kind: 'alert.resolved',
    channel_id: 'channel-1',
    event_id: ' event-9 ',
  }), 'http://localhost')

  assert.deepEqual(Object.fromEntries(filtered.searchParams), {
    page: '2',
    page_size: '100',
    status: 'failed',
    kind: 'alert.resolved',
    channel_id: 'channel-1',
    event_id: 'event-9',
  })
  assert.equal(
    alertDeliveryQueryPath({
      page: 1,
      page_size: 50,
      status: '',
      kind: '',
      channel_id: '',
      event_id: ' ',
    }),
    '/api/admin/alerts/deliveries?page=1&page_size=50',
  )
})
