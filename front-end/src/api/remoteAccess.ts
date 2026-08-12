import { api } from './http'
import type {
  RemoteAccessAvailability,
  RemoteAccessDeviceStatus,
  RemoteAccessDriverState,
  RemoteAccessSource,
  RemoteAccessStatus,
} from '../types/domain'

const availabilityValues = new Set<RemoteAccessAvailability>([
  'ready',
  'degraded',
  'unavailable',
  'unknown',
])
const sourceValues = new Set<RemoteAccessSource>(['physical', 'virtual', 'none', 'unknown'])
const driverStateValues = new Set<RemoteAccessDriverState>([
  'active',
  'standby',
  'missing',
  'reboot_required',
  'unhealthy',
  'unsupported',
  'unknown',
])

export async function getRemoteAccessStatus(instanceId: string) {
  const response = await api<unknown>(
    `/api/admin/instances/${encodeURIComponent(instanceId)}/remote-access/status`,
  )
  return normalizeRemoteAccessStatus(unwrap(response))
}

export function normalizeRemoteAccessStatus(value: unknown): RemoteAccessStatus {
  const record = asRecord(value)
  const accessMode = record.access_mode === 'local_consent' ? 'required' : record.access_mode
  return {
    protocol_supported: record.protocol_supported === true,
    status_supported: record.status_supported === true,
    online: record.online === true,
    access_mode: enumValue(accessMode, ['required', 'unattended'], 'unknown'),
    fallback_mode: enumValue(
      record.fallback_mode,
      ['auto', 'disabled', 'physical_only'],
      'unknown',
    ),
    display: normalizeDeviceStatus(record.display),
    audio: normalizeDeviceStatus(record.audio),
    reboot_required: record.reboot_required === true,
    checked_at: finiteNumber(record.checked_at),
  }
}

export function remoteAccessCodeLabel(code: string | null | undefined) {
  if (!code) return ''
  const labels: Record<string, string> = {
    display_unavailable: '当前没有可用显示设备',
    no_display_device: '未检测到物理显示器，虚拟显示器尚未就绪',
    virtual_display_preparing: '正在准备虚拟显示器',
    virtual_display_driver_missing: '虚拟显示驱动未安装',
    virtual_display_driver_unhealthy: '虚拟显示驱动运行异常',
    virtual_display_reboot_required: '虚拟显示驱动需要重启后生效',
    audio_unavailable: '当前没有可用音频播放设备',
    audio_endpoint_unavailable: '未检测到可用音频播放端点',
    virtual_audio_driver_missing: '虚拟音频驱动未安装',
    virtual_audio_driver_unhealthy: '虚拟音频驱动运行异常',
    virtual_audio_reboot_required: '虚拟音频驱动需要重启后生效',
    virtual_device_not_ready: '虚拟设备尚未就绪',
    virtual_devices_disabled: '虚拟设备兜底已停用',
    driver_bundle_missing: '当前 Agent 未内置虚拟设备驱动',
    windows_audio_disabled: 'Windows Audio 服务未运行',
    unsupported_platform: '当前 Windows 版本不支持虚拟设备兜底',
    instance_offline: '实例离线，状态可能已过期',
  }
  return labels[code] || '远程访问设备状态异常'
}

function normalizeDeviceStatus(value: unknown): RemoteAccessDeviceStatus {
  const record = asRecord(value)
  const availability = typeof record.availability === 'string'
    && availabilityValues.has(record.availability as RemoteAccessAvailability)
    ? record.availability as RemoteAccessAvailability
    : 'unknown'
  const source = typeof record.source === 'string'
    && sourceValues.has(record.source as RemoteAccessSource)
    ? record.source as RemoteAccessSource
    : 'unknown'
  const driverState = typeof record.driver_state === 'string'
    && driverStateValues.has(record.driver_state as RemoteAccessDriverState)
    ? record.driver_state as RemoteAccessDriverState
    : 'unknown'
  return {
    availability,
    source,
    driver_state: driverState,
    driver_version: stringOrNull(record.driver_version),
    code: stringOrNull(record.code),
  }
}

function unwrap(value: unknown) {
  const record = asRecord(value)
  if ('data' in record) return record.data
  if ('result' in record) return record.result
  return value
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {}
}

function enumValue<T extends string>(value: unknown, values: readonly T[], fallback: T): T {
  return typeof value === 'string' && values.includes(value as T) ? value as T : fallback
}

function stringOrNull(value: unknown) {
  return typeof value === 'string' && value.trim() ? value : null
}

function finiteNumber(value: unknown) {
  return typeof value === 'number' && Number.isFinite(value) ? value : null
}
