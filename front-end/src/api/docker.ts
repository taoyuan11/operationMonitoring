import { api } from './http'
import type {
  DockerComposeProject,
  DockerComposeRequest,
  DockerComposeValidation,
  DockerContainer,
  DockerContainerCreateInput,
  DockerContainerDetail,
  DockerContainerStats,
  DockerDiskUsage,
  DockerImage,
  DockerNetwork,
  DockerOperationResult,
  DockerStatus,
  DockerVolume,
} from '../types/docker'

type Envelope<T> = T | { data: T } | { result: T }

function base(instanceId: string) {
  return `/api/admin/instances/${encodeURIComponent(instanceId)}/docker`
}

function unwrap<T>(value: Envelope<T>): T {
  if (value && typeof value === 'object' && !Array.isArray(value)) {
    if ('data' in value) return value.data
    if ('result' in value) return value.result
  }
  return value as T
}

function unwrapList<T>(value: unknown, keys: string[]): T[] {
  const unwrapped = unwrap(value as Envelope<unknown>)
  if (Array.isArray(unwrapped)) return unwrapped as T[]
  if (unwrapped && typeof unwrapped === 'object') {
    for (const key of keys) {
      const candidate = (unwrapped as Record<string, unknown>)[key]
      if (Array.isArray(candidate)) return candidate as T[]
    }
    return [unwrapped as T]
  }
  return []
}

async function json<T>(path: string, options?: RequestInit) {
  return unwrap(await api<Envelope<T>>(path, options))
}

export async function getDockerStatus(instanceId: string) {
  return json<DockerStatus>(`${base(instanceId)}/status`)
}

export async function listDockerContainers(instanceId: string) {
  return unwrapList<Record<string, unknown>>(
    await api<unknown>(`${base(instanceId)}/containers?all=true`),
    ['containers', 'items'],
  ).map(normalizeContainer)
}

export function getDockerContainer(instanceId: string, containerId: string) {
  return json<DockerContainerDetail>(
    `${base(instanceId)}/containers/${encodeURIComponent(containerId)}`,
  )
}

export function getDockerContainerStats(instanceId: string, containerId: string) {
  return api<unknown>(`${base(instanceId)}/containers/${encodeURIComponent(containerId)}/stats`)
    .then((value) => normalizeContainerStats(unwrapListOrSingle(value)))
}

export function createDockerContainer(instanceId: string, input: DockerContainerCreateInput) {
  return json<DockerOperationResult>(`${base(instanceId)}/containers`, {
    method: 'POST',
    body: JSON.stringify(input),
  })
}

export function runDockerContainerAction(
  instanceId: string,
  containerId: string,
  action: 'start' | 'stop' | 'restart' | 'kill' | 'pause' | 'unpause',
  options: { timeout_seconds?: number; signal?: string } = {},
) {
  return json<DockerOperationResult>(
    `${base(instanceId)}/containers/${encodeURIComponent(containerId)}/actions/${action}`,
    { method: 'POST', body: JSON.stringify(options) },
  )
}

export function renameDockerContainer(instanceId: string, containerId: string, name: string) {
  return json<DockerOperationResult>(
    `${base(instanceId)}/containers/${encodeURIComponent(containerId)}/actions/rename`,
    { method: 'POST', body: JSON.stringify({ name }) },
  )
}

export function deleteDockerContainer(
  instanceId: string,
  containerId: string,
  options: { force?: boolean; remove_volumes?: boolean } = {},
) {
  const query = new URLSearchParams({
    force: String(options.force ?? false),
    remove_volumes: String(options.remove_volumes ?? false),
  })
  return json<DockerOperationResult>(
    `${base(instanceId)}/containers/${encodeURIComponent(containerId)}?${query}`,
    { method: 'DELETE' },
  )
}

export async function listDockerImages(instanceId: string) {
  return unwrapList<Record<string, unknown>>(
    await api<unknown>(`${base(instanceId)}/images`),
    ['images', 'items'],
  ).map(normalizeImage)
}

export function getDockerImage(instanceId: string, imageId: string) {
  return json<DockerImage>(`${base(instanceId)}/images/${encodeURIComponent(imageId)}`)
}

export function pullDockerImage(instanceId: string, reference: string) {
  return json<DockerOperationResult>(`${base(instanceId)}/images/pull`, {
    method: 'POST',
    body: JSON.stringify({ reference }),
  })
}

export function tagDockerImage(instanceId: string, imageId: string, repository: string, tag: string) {
  return json<DockerOperationResult>(`${base(instanceId)}/images/${encodeURIComponent(imageId)}/tag`, {
    method: 'POST',
    body: JSON.stringify({ repository, tag }),
  })
}

export function deleteDockerImage(instanceId: string, imageId: string, force = false) {
  const query = new URLSearchParams({ force: String(force) })
  return json<DockerOperationResult>(
    `${base(instanceId)}/images/${encodeURIComponent(imageId)}?${query}`,
    { method: 'DELETE' },
  )
}

export async function listDockerNetworks(instanceId: string) {
  return unwrapList<Record<string, unknown>>(
    await api<unknown>(`${base(instanceId)}/networks`),
    ['networks', 'items'],
  ).map(normalizeNetwork)
}

export function getDockerNetwork(instanceId: string, networkId: string) {
  return json<DockerNetwork>(`${base(instanceId)}/networks/${encodeURIComponent(networkId)}`)
}

export function createDockerNetwork(instanceId: string, name: string, driver: string, internal: boolean) {
  return json<DockerOperationResult>(`${base(instanceId)}/networks`, {
    method: 'POST',
    body: JSON.stringify({ name, driver, internal }),
  })
}

export function connectDockerNetwork(
  instanceId: string,
  networkId: string,
  container: string,
  aliases: string[],
) {
  return json<DockerOperationResult>(
    `${base(instanceId)}/networks/${encodeURIComponent(networkId)}/connect`,
    { method: 'POST', body: JSON.stringify({ container, aliases }) },
  )
}

export function disconnectDockerNetwork(
  instanceId: string,
  networkId: string,
  container: string,
  force: boolean,
) {
  return json<DockerOperationResult>(
    `${base(instanceId)}/networks/${encodeURIComponent(networkId)}/disconnect`,
    { method: 'POST', body: JSON.stringify({ container, force }) },
  )
}

export function deleteDockerNetwork(instanceId: string, networkId: string) {
  return json<DockerOperationResult>(
    `${base(instanceId)}/networks/${encodeURIComponent(networkId)}`,
    { method: 'DELETE' },
  )
}

export async function listDockerVolumes(instanceId: string) {
  return unwrapList<Record<string, unknown>>(
    await api<unknown>(`${base(instanceId)}/volumes`),
    ['volumes', 'items'],
  ).map(normalizeVolume)
}

export function getDockerVolume(instanceId: string, name: string) {
  return json<DockerVolume>(`${base(instanceId)}/volumes/${encodeURIComponent(name)}`)
}

export function createDockerVolume(instanceId: string, name: string, driver: string) {
  return json<DockerOperationResult>(`${base(instanceId)}/volumes`, {
    method: 'POST',
    body: JSON.stringify({ name, driver }),
  })
}

export function deleteDockerVolume(instanceId: string, name: string, force = false) {
  const query = new URLSearchParams({ force: String(force) })
  return json<DockerOperationResult>(`${base(instanceId)}/volumes/${encodeURIComponent(name)}?${query}`, {
    method: 'DELETE',
  })
}

export async function listDockerComposeProjects(instanceId: string) {
  return unwrapList<Record<string, unknown>>(
    await api<unknown>(`${base(instanceId)}/compose/projects`),
    ['projects', 'items'],
  ).map(normalizeComposeProject)
}

export function getDockerComposeProject(instanceId: string, projectName: string) {
  return json<DockerComposeProject>(
    `${base(instanceId)}/compose/projects/${encodeURIComponent(projectName)}`,
  )
}

export function validateDockerCompose(instanceId: string, input: DockerComposeRequest) {
  return json<DockerComposeValidation>(`${base(instanceId)}/compose/validate`, {
    method: 'POST',
    body: JSON.stringify(input),
  })
}

export function deployDockerCompose(instanceId: string, input: DockerComposeRequest) {
  return json<DockerOperationResult>(`${base(instanceId)}/compose/deploy`, {
    method: 'POST',
    body: JSON.stringify(input),
  })
}

export function runDockerComposeAction(
  instanceId: string,
  projectName: string,
  action: 'pull' | 'up' | 'start' | 'stop' | 'restart' | 'down',
  options: {
    services?: string[]
    profiles?: string[]
    config_digest?: string
    remove_volumes?: boolean
    confirm_risks?: boolean
  } = {},
) {
  return json<DockerOperationResult>(
    `${base(instanceId)}/compose/projects/${encodeURIComponent(projectName)}/actions/${action}`,
    { method: 'POST', body: JSON.stringify(options) },
  )
}

export function getDockerDiskUsage(instanceId: string) {
  return api<unknown>(`${base(instanceId)}/system/df`).then(normalizeDiskUsage)
}

export function pruneDockerSystem(
  instanceId: string,
  options: { all: boolean; volumes: boolean; confirm: boolean },
) {
  return json<DockerOperationResult>(`${base(instanceId)}/system/prune`, {
    method: 'POST',
    body: JSON.stringify(options),
  })
}

export function pruneDockerResource(
  instanceId: string,
  resource: 'containers' | 'images' | 'networks' | 'volumes',
  all = false,
) {
  return json<DockerOperationResult>(`${base(instanceId)}/prune/${resource}`, {
    method: 'POST',
    body: JSON.stringify({ all, confirm: true }),
  })
}

export function dockerWebSocketUrl(
  instanceId: string,
  path: string,
  params: Record<string, string | number | boolean | null | undefined> = {},
) {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const query = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value !== null && value !== undefined && value !== '') query.set(key, String(value))
  }
  const suffix = query.size ? `?${query}` : ''
  return `${protocol}//${window.location.host}${base(instanceId)}/${path}${suffix}`
}

function unwrapListOrSingle(value: unknown): Record<string, unknown> {
  const unwrapped = unwrap(value as Envelope<unknown>)
  if (Array.isArray(unwrapped)) return asRecord(unwrapped[0])
  return asRecord(unwrapped)
}

function normalizeContainer(raw: Record<string, unknown>): DockerContainer {
  const names = stringValue(raw, 'names', 'Names')
    .split(',')
    .map((name) => name.trim().replace(/^\//, ''))
    .filter(Boolean)
  const labels = objectValue(raw, 'labels') || parseKeyValueList(stringValue(raw, 'Labels'))
  const networks = stringValue(raw, 'Networks').split(',').map((item) => item.trim()).filter(Boolean)
  return {
    ...raw,
    id: stringValue(raw, 'id', 'ID'),
    name: stringValue(raw, 'name') || names[0],
    names,
    image: stringValue(raw, 'image', 'Image'),
    image_id: stringValue(raw, 'image_id', 'ImageID') || undefined,
    command: stringValue(raw, 'command', 'Command') || undefined,
    created: value(raw, 'created', 'CreatedAt', 'Created') as string | number | undefined,
    state: stringValue(raw, 'state', 'State'),
    status: stringValue(raw, 'status', 'Status'),
    ports: Array.isArray(raw.ports) ? raw.ports as DockerContainer['ports'] : undefined,
    ports_text: stringValue(raw, 'ports_text', 'Ports') || undefined,
    mounts: Array.isArray(raw.mounts) ? raw.mounts as DockerContainer['mounts'] : undefined,
    networks: networks.length ? networks : raw.networks as DockerContainer['networks'],
    labels,
  }
}

function normalizeContainerStats(raw: Record<string, unknown>): DockerContainerStats {
  const [memoryUsage, memoryLimit] = parseSizePair(stringValue(raw, 'MemUsage'))
  const [networkRx, networkTx] = parseSizePair(stringValue(raw, 'NetIO'))
  const [blockRead, blockWrite] = parseSizePair(stringValue(raw, 'BlockIO'))
  return {
    container_id: stringValue(raw, 'container_id', 'ID', 'Container') || undefined,
    cpu_percent: numberValue(raw.cpu_percent) ?? parsePercent(raw.CPUPerc),
    memory_usage: numberValue(raw.memory_usage) ?? memoryUsage,
    memory_limit: numberValue(raw.memory_limit) ?? memoryLimit,
    memory_percent: numberValue(raw.memory_percent) ?? parsePercent(raw.MemPerc),
    network_rx: numberValue(raw.network_rx) ?? networkRx,
    network_tx: numberValue(raw.network_tx) ?? networkTx,
    block_read: numberValue(raw.block_read) ?? blockRead,
    block_write: numberValue(raw.block_write) ?? blockWrite,
    pids: numberValue(raw.pids) ?? numberValue(raw.PIDs),
  }
}

function normalizeImage(raw: Record<string, unknown>): DockerImage {
  const repository = stringValue(raw, 'Repository')
  const tag = stringValue(raw, 'Tag')
  const cliTag = repository && repository !== '<none>'
    ? `${repository}:${tag || 'latest'}`
    : '<none>:<none>'
  return {
    ...raw,
    id: stringValue(raw, 'id', 'ID'),
    repo_tags: stringArray(raw.repo_tags, raw.RepoTags, raw.repoTags) || [cliTag],
    repo_digests: stringArray(raw.repo_digests, raw.RepoDigests)
      || (stringValue(raw, 'Digest') ? [`${repository}@${stringValue(raw, 'Digest')}`] : []),
    parent_id: stringValue(raw, 'parent_id', 'ParentID') || undefined,
    created: value(raw, 'created', 'CreatedAt', 'CreatedSince') as string | number | undefined,
    size: numberValue(raw.size) ?? parseByteSize(raw.Size) ?? undefined,
    shared_size: numberValue(raw.shared_size) ?? parseByteSize(raw.SharedSize) ?? undefined,
    virtual_size: numberValue(raw.virtual_size) ?? parseByteSize(raw.VirtualSize) ?? undefined,
    containers: numberValue(raw.containers) ?? numberValue(raw.Containers) ?? undefined,
    labels: objectValue(raw, 'labels') || parseKeyValueList(stringValue(raw, 'Labels')),
  }
}

function normalizeNetwork(raw: Record<string, unknown>): DockerNetwork {
  return {
    ...raw,
    id: stringValue(raw, 'id', 'ID'),
    name: stringValue(raw, 'name', 'Name'),
    driver: stringValue(raw, 'driver', 'Driver'),
    scope: stringValue(raw, 'scope', 'Scope') || undefined,
    internal: booleanValue(value(raw, 'internal', 'Internal')),
    attachable: booleanValue(value(raw, 'attachable', 'Attachable')),
    ingress: booleanValue(value(raw, 'ingress', 'Ingress')),
    labels: objectValue(raw, 'labels') || parseKeyValueList(stringValue(raw, 'Labels')),
    created: value(raw, 'created', 'CreatedAt') as string | number | undefined,
  }
}

function normalizeVolume(raw: Record<string, unknown>): DockerVolume {
  return {
    ...raw,
    name: stringValue(raw, 'name', 'Name'),
    driver: stringValue(raw, 'driver', 'Driver'),
    mountpoint: stringValue(raw, 'mountpoint', 'Mountpoint') || undefined,
    scope: stringValue(raw, 'scope', 'Scope') || undefined,
    labels: objectValue(raw, 'labels') || parseKeyValueList(stringValue(raw, 'Labels')),
    created_at: value(raw, 'created_at', 'CreatedAt') as string | number | undefined,
    usage_data: raw.usage_data as DockerVolume['usage_data'],
  }
}

function normalizeComposeProject(raw: Record<string, unknown>): DockerComposeProject {
  const configFiles = stringArray(raw.config_files, raw.ConfigFiles)
    || stringValue(raw, 'ConfigFiles').split(',').map((item) => item.trim()).filter(Boolean)
  return {
    ...raw,
    name: stringValue(raw, 'name', 'Name'),
    status: stringValue(raw, 'status', 'Status') || undefined,
    config_files: configFiles,
    working_dir: stringValue(raw, 'working_dir', 'WorkingDir') || undefined,
    services: Array.isArray(raw.services) ? raw.services as DockerComposeProject['services'] : undefined,
    containers: numberValue(raw.containers) ?? undefined,
    running: numberValue(raw.running) ?? undefined,
  }
}

function normalizeDiskUsage(value: unknown): DockerDiskUsage {
  const unwrapped = unwrap(value as Envelope<unknown>)
  if (!Array.isArray(unwrapped)) {
    const record = asRecord(unwrapped)
    if (!valueFrom(record, 'type', 'Type')) return record as DockerDiskUsage
  }
  const rows = (Array.isArray(unwrapped) ? unwrapped : [unwrapped]).map(asRecord)
  const result: DockerDiskUsage = { rows }
  for (const row of rows) {
    const type = stringValue(row, 'type', 'Type').toLowerCase()
    const count = numberValue(valueFrom(row, 'total_count', 'TotalCount'))
    const size = parseByteSize(valueFrom(row, 'size', 'Size'))
    const reclaimable = parseByteSize(
      String(valueFrom(row, 'reclaimable', 'Reclaimable') || '').split(' ')[0],
    )
    result.reclaimable_size = (result.reclaimable_size || 0) + (reclaimable || 0)
    if (type.includes('image')) {
      result.image_count = count ?? undefined
      result.images_size = size ?? undefined
    } else if (type.includes('container')) {
      result.container_count = count ?? undefined
      result.containers_size = size ?? undefined
    } else if (type.includes('volume')) {
      result.volume_count = count ?? undefined
      result.volumes_size = size ?? undefined
    }
  }
  return result
}

function value(raw: Record<string, unknown>, ...keys: string[]) {
  return valueFrom(raw, ...keys)
}

function valueFrom(raw: Record<string, unknown>, ...keys: string[]) {
  for (const key of keys) {
    if (raw[key] !== undefined && raw[key] !== null) return raw[key]
  }
  return undefined
}

function stringValue(raw: Record<string, unknown>, ...keys: string[]) {
  const item = valueFrom(raw, ...keys)
  return typeof item === 'string' ? item : item == null ? '' : String(item)
}

function objectValue(raw: Record<string, unknown>, key: string) {
  const item = raw[key]
  return item && typeof item === 'object' && !Array.isArray(item)
    ? item as Record<string, string>
    : undefined
}

function stringArray(...values: unknown[]): string[] | undefined {
  for (const item of values) {
    if (Array.isArray(item)) return item.map(String)
  }
  return undefined
}

function numberValue(item: unknown): number | null {
  if (typeof item === 'number' && Number.isFinite(item)) return item
  if (typeof item === 'string' && item.trim() && Number.isFinite(Number(item))) return Number(item)
  return null
}

function booleanValue(item: unknown) {
  return item === true || item === 1 || (typeof item === 'string' && ['true', 'yes', '1'].includes(item.toLowerCase()))
}

function parsePercent(item: unknown) {
  if (typeof item !== 'string') return null
  return numberValue(item.replace('%', '').trim())
}

function parseSizePair(item: string): [number | null, number | null] {
  const [left = '', right = ''] = item.split('/').map((part) => part.trim())
  return [parseByteSize(left), parseByteSize(right)]
}

function parseByteSize(item: unknown): number | null {
  if (typeof item === 'number' && Number.isFinite(item)) return item
  if (typeof item !== 'string') return null
  const match = item.trim().replace(/,/g, '').match(/^([\d.]+)\s*([kmgtpe]?i?b)?$/i)
  if (!match) return null
  const numeric = Number(match[1])
  const unit = (match[2] || 'b').toLowerCase()
  const powers: Record<string, number> = {
    b: 0,
    kb: 1,
    kib: 1,
    mb: 2,
    mib: 2,
    gb: 3,
    gib: 3,
    tb: 4,
    tib: 4,
    pb: 5,
    pib: 5,
    eb: 6,
    eib: 6,
  }
  return Number.isFinite(numeric) && unit in powers ? numeric * 1024 ** powers[unit] : null
}

function parseKeyValueList(item: string) {
  if (!item) return undefined
  return Object.fromEntries(item.split(',').map((entry) => entry.split('=', 2)).filter(([key]) => key))
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {}
}
