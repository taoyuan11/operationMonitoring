import { api } from './http'
import type { TerminalSessionStatus, TerminalShellListResponse } from '../types/domain'

export function terminalShellsPath(instanceId: string) {
  return `/api/admin/instances/${encodeURIComponent(instanceId)}/terminal/shells`
}

export function listTerminalShells(instanceId: string) {
  return api<TerminalShellListResponse>(terminalShellsPath(instanceId))
}

export function terminalWebSocketPath(instanceId: string, shellProgram: string | null) {
  const path = `/api/admin/instances/${encodeURIComponent(instanceId)}/terminal/ws`
  if (!shellProgram) return path
  const query = new URLSearchParams({ shell: shellProgram })
  return `${path}?${query.toString()}`
}

export function terminalWebSocketUrl(instanceId: string, shellProgram: string | null) {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${window.location.host}${terminalWebSocketPath(instanceId, shellProgram)}`
}

export function validCustomShellProgram(program: string, targetOs = '') {
  const encodedLength = new TextEncoder().encode(program).length
  if (!program || encodedLength > 1024 || program.trim() !== program) return false
  if (/[\u0000-\u001f\u007f-\u009f]/u.test(program)) return false
  const windowsAbsolute = /^[a-z]:[\\/]/iu.test(program)
    || /^\\\\[^\\/]+[\\/][^\\/]+/u.test(program)
  const posixAbsolute = program.startsWith('/')
  const normalizedOs = targetOs.toLocaleLowerCase()
  const absolute = normalizedOs.includes('windows')
    ? windowsAbsolute
    : normalizedOs
      ? posixAbsolute
      : windowsAbsolute || posixAbsolute
  if (absolute) return true
  return !program.includes('/') && !program.includes('\\') && !/\s/u.test(program)
}

export function activeTerminalSessionCount(
  sessions: Array<{ instanceId: string; status: TerminalSessionStatus }>,
  instanceId: string,
) {
  return sessions.filter((session) =>
    session.instanceId === instanceId
      && (session.status === 'opening' || session.status === 'ready'),
  ).length
}
