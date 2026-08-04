import type { AuditExportFormat, AuditQuery } from '../types/domain'

export function auditQueryPath(query: AuditQuery, options: { exportFormat?: AuditExportFormat } = {}) {
  const params = new URLSearchParams()
  if (query.from !== null) params.set('from', String(query.from))
  if (query.to !== null) params.set('to', String(query.to))
  if (!options.exportFormat) {
    params.set('page', String(query.page))
    params.set('page_size', String(query.page_size))
  }
  const filters: Array<[keyof AuditQuery, string]> = [
    ['user_id', query.user_id],
    ['actor', query.actor],
    ['category', query.category],
    ['action', query.action],
    ['instance_id', query.instance_id],
    ['status', query.status],
    ['source_ip', query.source_ip],
    ['request_id', query.request_id],
    ['keyword', query.keyword],
  ]
  for (const [key, value] of filters) {
    if (value) params.set(key, value)
  }
  if (options.exportFormat) params.set('format', options.exportFormat)
  const queryString = params.toString()
  const endpoint = options.exportFormat ? '/api/admin/audit/export' : '/api/admin/audit'
  return `${endpoint}${queryString ? `?${queryString}` : ''}`
}

export async function downloadAuditExport(
  query: AuditQuery,
  format: AuditExportFormat,
  signal?: AbortSignal,
) {
  const response = await fetch(auditQueryPath(query, { exportFormat: format }), {
    credentials: 'include',
    signal,
  })
  if (!response.ok) {
    const message = await response
      .json()
      .then((body: { message?: string }) => body.message)
      .catch(() => response.statusText)
    throw new Error(message || response.statusText || '导出失败')
  }
  return response.blob()
}

export function auditExportFileName(format: AuditExportFormat) {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-')
  return `audit-${stamp}.${format}`
}
