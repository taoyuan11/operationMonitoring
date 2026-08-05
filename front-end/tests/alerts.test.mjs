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
const {
  alertDeliveryQueryPath,
  alertEventQueryPath,
  createAlertChannel,
  deleteAlertChannel,
  listAlertChannels,
  normalizeAlertChannel,
  parseAlertEmailRecipients,
  testAlertChannel,
  updateAlertChannel,
} = await import(moduleUrl)

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

test('normalizes legacy webhook channel responses', () => {
  const channel = normalizeAlertChannel({
    id: 'channel-1',
    name: 'Legacy webhook',
    masked_url: 'https://hooks.example.test/...',
    header_names: [],
    has_secret: false,
    enabled: true,
    created_at: 1,
    updated_at: 2,
  })

  assert.equal(channel.channel_type, 'generic_webhook')
  assert.equal(channel.has_password, false)
})

test('normalizes and de-duplicates email recipients', () => {
  assert.deepEqual(
    parseAlertEmailRecipients(' oncall@example.test,ops@example.test\n ONCALL@example.test; '),
    ['oncall@example.test', 'ops@example.test'],
  )
})

test('keeps Telegram chat identifiers when normalizing channel responses', () => {
  const channel = normalizeAlertChannel({
    id: 'telegram-1',
    name: 'Telegram on-call',
    channel_type: 'telegram',
    masked_url: 'https://api.telegram.org/...',
    header_names: [],
    has_secret: false,
    chat_id: '-1001234567890',
    enabled: true,
    created_at: 1,
    updated_at: 2,
  })

  assert.equal(channel.channel_type, 'telegram')
  assert.equal(channel.chat_id, '-1001234567890')
})

test('uses the generic channels route for channel lifecycle actions', async () => {
  const calls = []
  const channel = {
    id: 'channel-1',
    name: 'Mail on-call',
    channel_type: 'email',
    masked_url: '',
    header_names: [],
    has_secret: false,
    smtp_host: 'smtp.example.test',
    smtp_port: 587,
    security: 'starttls',
    username: 'alerts@example.test',
    from_address: 'alerts@example.test',
    from_name: 'Operations',
    recipients: ['oncall@example.test'],
    has_password: true,
    enabled: true,
    created_at: 1,
    updated_at: 2,
  }
  const request = async (path, options = {}) => {
    calls.push([path, options.method || 'GET'])
    if (path.includes('?')) {
      return { items: [channel], page: 2, page_size: 25, total: 1, pages: 1 }
    }
    return channel
  }
  const payload = {
    name: channel.name,
    channel_type: 'email',
    clear_secret: false,
    clear_password: false,
    enabled: true,
  }

  await listAlertChannels(2, 25, request)
  await createAlertChannel(payload, request)
  await updateAlertChannel(channel.id, payload, request)
  await deleteAlertChannel(channel.id, request)
  await testAlertChannel(channel.id, request)

  assert.deepEqual(calls, [
    ['/api/admin/alerts/channels?page=2&page_size=25', 'GET'],
    ['/api/admin/alerts/channels', 'POST'],
    ['/api/admin/alerts/channels/channel-1', 'PUT'],
    ['/api/admin/alerts/channels/channel-1', 'DELETE'],
    ['/api/admin/alerts/channels/channel-1/test', 'POST'],
  ])
})
