import { api } from './http'
import type {
  AlertDelivery,
  AlertDeliveryDetail,
  AlertDeliveryQuery,
  AlertEvent,
  AlertEventDetail,
  AlertEventQuery,
  AlertMaintenanceInput,
  AlertMaintenanceWindow,
  AlertPage,
  AlertRule,
  AlertRuleInput,
  AlertSummary,
  AlertWebhookChannel,
  AlertWebhookChannelInput,
} from '../types/domain'

export type AlertApiRequest = <T>(path: string, options?: RequestInit) => Promise<T>

const prefix = '/api/admin/alerts'

function pageQuery(page: number, pageSize: number) {
  const params = new URLSearchParams()
  params.set('page', String(page))
  params.set('page_size', String(pageSize))
  return params
}

function pathWithQuery(path: string, params: URLSearchParams) {
  const query = params.toString()
  return `${prefix}${path}${query ? `?${query}` : ''}`
}

function normalizePage<T>(page: AlertPage<T>, fallbackPage: number, fallbackPageSize: number): AlertPage<T> {
  return {
    items: page.items || [],
    page: page.page || fallbackPage,
    page_size: page.page_size || fallbackPageSize,
    total: page.total || 0,
    pages: page.pages || 0,
  }
}

export function alertEventQueryPath(query: AlertEventQuery) {
  const params = pageQuery(query.page, query.page_size)
  const filters: Array<[string, string]> = [
    ['status', query.status],
    ['severity', query.severity],
    ['metric', query.metric],
    ['instance_id', query.instance_id],
    ['suppressed', query.suppressed],
    ['search', query.search.trim()],
  ]
  for (const [key, value] of filters) {
    if (value) params.set(key, value)
  }
  if (query.from !== null) params.set('from', String(query.from))
  if (query.to !== null) params.set('to', String(query.to))
  return pathWithQuery('/events', params)
}

export function alertDeliveryQueryPath(query: AlertDeliveryQuery) {
  const params = pageQuery(query.page, query.page_size)
  const filters: Array<[string, string]> = [
    ['status', query.status],
    ['kind', query.kind],
    ['channel_id', query.channel_id],
    ['event_id', query.event_id.trim()],
  ]
  for (const [key, value] of filters) {
    if (value) params.set(key, value)
  }
  return pathWithQuery('/deliveries', params)
}

export function getAlertSummary(request: AlertApiRequest = api) {
  return request<AlertSummary>(`${prefix}/summary`)
}

export async function listAlertRules(page = 1, pageSize = 200, request: AlertApiRequest = api) {
  const response = await request<AlertPage<AlertRule>>(
    pathWithQuery('/rules', pageQuery(page, pageSize)),
  )
  return normalizePage(response, page, pageSize)
}

export function createAlertRule(payload: AlertRuleInput, request: AlertApiRequest = api) {
  return request<AlertRule>(`${prefix}/rules`, {
    method: 'POST',
    body: JSON.stringify(payload),
  })
}

export function updateAlertRule(id: string, payload: AlertRuleInput, request: AlertApiRequest = api) {
  return request<AlertRule>(`${prefix}/rules/${id}`, {
    method: 'PUT',
    body: JSON.stringify(payload),
  })
}

export function setAlertRuleEnabled(id: string, enabled: boolean, request: AlertApiRequest = api) {
  return request<AlertRule>(`${prefix}/rules/${id}/enabled`, {
    method: 'PATCH',
    body: JSON.stringify({ enabled }),
  })
}

export function deleteAlertRule(id: string, request: AlertApiRequest = api) {
  return request<void>(`${prefix}/rules/${id}`, { method: 'DELETE' })
}

export async function listAlertEvents(query: AlertEventQuery, request: AlertApiRequest = api) {
  const response = await request<AlertPage<AlertEvent>>(alertEventQueryPath(query))
  return normalizePage(response, query.page, query.page_size)
}

export async function getAlertEvent(id: string, request: AlertApiRequest = api) {
  const response = await request<
    AlertEventDetail | { event: AlertEvent; timeline?: AlertEventDetail['timeline']; deliveries?: AlertDelivery[] }
  >(`${prefix}/events/${id}`)
  return normalizeEventDetail(response)
}

function normalizeEventDetail(
  response: AlertEventDetail | { event: AlertEvent; timeline?: AlertEventDetail['timeline']; deliveries?: AlertDelivery[] },
) {
  if ('event' in response) {
    return {
      ...response.event,
      timeline: response.timeline || [],
      deliveries: response.deliveries || [],
    }
  }
  return {
    ...response,
    timeline: response.timeline || [],
    deliveries: response.deliveries || [],
  }
}

export function acknowledgeAlertEvent(id: string, note: string, request: AlertApiRequest = api) {
  return request<
    AlertEventDetail | { event: AlertEvent; timeline?: AlertEventDetail['timeline']; deliveries?: AlertDelivery[] }
  >(`${prefix}/events/${id}/acknowledge`, {
    method: 'POST',
    body: JSON.stringify({ note }),
  }).then(normalizeEventDetail)
}

function normalizeMaintenance(
  item: AlertMaintenanceWindow | { window: Omit<AlertMaintenanceWindow, 'target_ids'>; target_ids?: string[] },
): AlertMaintenanceWindow {
  if ('window' in item) return { ...item.window, target_ids: item.target_ids || [] }
  return { ...item, target_ids: item.target_ids || [] }
}

export async function listAlertMaintenance(page = 1, pageSize = 200, request: AlertApiRequest = api) {
  const response = await request<
    AlertPage<AlertMaintenanceWindow | { window: Omit<AlertMaintenanceWindow, 'target_ids'>; target_ids?: string[] }>
  >(pathWithQuery('/maintenance-windows', pageQuery(page, pageSize)))
  return normalizePage(
    { ...response, items: (response.items || []).map(normalizeMaintenance) },
    page,
    pageSize,
  )
}

export async function createAlertMaintenance(payload: AlertMaintenanceInput, request: AlertApiRequest = api) {
  const response = await request<
    AlertMaintenanceWindow | { window: Omit<AlertMaintenanceWindow, 'target_ids'>; target_ids?: string[] }
  >(`${prefix}/maintenance-windows`, { method: 'POST', body: JSON.stringify(payload) })
  return normalizeMaintenance(response)
}

export async function updateAlertMaintenance(
  id: string,
  payload: AlertMaintenanceInput,
  request: AlertApiRequest = api,
) {
  const response = await request<
    AlertMaintenanceWindow | { window: Omit<AlertMaintenanceWindow, 'target_ids'>; target_ids?: string[] }
  >(`${prefix}/maintenance-windows/${id}`, { method: 'PUT', body: JSON.stringify(payload) })
  return normalizeMaintenance(response)
}

export function deleteAlertMaintenance(id: string, request: AlertApiRequest = api) {
  return request<void>(`${prefix}/maintenance-windows/${id}`, { method: 'DELETE' })
}

export async function listAlertChannels(page = 1, pageSize = 200, request: AlertApiRequest = api) {
  const response = await request<AlertPage<AlertWebhookChannel>>(
    pathWithQuery('/webhook-channels', pageQuery(page, pageSize)),
  )
  return normalizePage(response, page, pageSize)
}

export function createAlertChannel(payload: AlertWebhookChannelInput, request: AlertApiRequest = api) {
  return request<AlertWebhookChannel>(`${prefix}/webhook-channels`, {
    method: 'POST',
    body: JSON.stringify(payload),
  })
}

export function updateAlertChannel(
  id: string,
  payload: AlertWebhookChannelInput,
  request: AlertApiRequest = api,
) {
  return request<AlertWebhookChannel>(`${prefix}/webhook-channels/${id}`, {
    method: 'PUT',
    body: JSON.stringify(payload),
  })
}

export function deleteAlertChannel(id: string, request: AlertApiRequest = api) {
  return request<void>(`${prefix}/webhook-channels/${id}`, { method: 'DELETE' })
}

export function testAlertChannel(id: string, request: AlertApiRequest = api) {
  return request<AlertDelivery>(`${prefix}/webhook-channels/${id}/test`, { method: 'POST' })
}

export async function listAlertDeliveries(query: AlertDeliveryQuery, request: AlertApiRequest = api) {
  const response = await request<AlertPage<AlertDelivery>>(alertDeliveryQueryPath(query))
  return normalizePage(response, query.page, query.page_size)
}

export async function getAlertDelivery(id: string, request: AlertApiRequest = api) {
  const response = await request<
    AlertDeliveryDetail | { delivery: AlertDelivery; attempts?: AlertDeliveryDetail['attempts'] }
  >(`${prefix}/deliveries/${id}`)
  if ('delivery' in response) return { ...response.delivery, attempts: response.attempts || [] }
  return { ...response, attempts: response.attempts || [] }
}

export function retryAlertDelivery(id: string, request: AlertApiRequest = api) {
  return request<
    AlertDeliveryDetail | { delivery: AlertDelivery; attempts?: AlertDeliveryDetail['attempts'] }
  >(`${prefix}/deliveries/${id}/retry`, { method: 'POST' }).then((response) =>
    'delivery' in response
      ? { ...response.delivery, attempts: response.attempts || [] }
      : { ...response, attempts: response.attempts || [] },
  )
}
