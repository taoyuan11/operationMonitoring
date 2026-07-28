export type DockerAvailability =
  | 'unknown'
  | 'not_installed'
  | 'daemon_unreachable'
  | 'permission_denied'
  | 'unsupported_version'
  | 'ready'
  | 'error'

export type DockerStatus = {
  status: DockerAvailability
  protocol_supported: boolean
  installed: boolean
  manageable: boolean
  online: boolean
  cli_version: string | null
  engine_version: string | null
  api_version: string | null
  compose_version: string | null
  diagnostic: string | null
  checked_at: number | null
}

export type DockerPort = {
  ip?: string | null
  private_port?: number
  public_port?: number | null
  type?: 'tcp' | 'udp' | string
  container_port?: number
  host_port?: number | null
  host_ip?: string | null
}

export type DockerContainer = {
  id: string
  name?: string
  names?: string[]
  image: string
  image_id?: string
  command?: string | string[]
  created?: number | string
  state: string
  status: string
  ports?: DockerPort[]
  ports_text?: string
  mounts?: Array<string | Record<string, unknown>>
  networks?: string[] | Record<string, unknown>
  labels?: Record<string, string>
}

export type DockerContainerStats = {
  container_id?: string
  cpu_percent?: number | null
  memory_usage?: number | null
  memory_limit?: number | null
  memory_percent?: number | null
  network_rx?: number | null
  network_tx?: number | null
  block_read?: number | null
  block_write?: number | null
  pids?: number | null
}

export type DockerContainerDetail = DockerContainer & {
  config?: Record<string, unknown>
  host_config?: Record<string, unknown>
  network_settings?: Record<string, unknown>
  path?: string
  args?: string[]
  started_at?: string | number | null
  finished_at?: string | number | null
  restart_count?: number
  [key: string]: unknown
}

export type DockerImage = {
  id: string
  repo_tags?: string[]
  repo_digests?: string[]
  parent_id?: string
  created?: number | string
  size?: number
  shared_size?: number
  virtual_size?: number
  containers?: number
  labels?: Record<string, string>
  [key: string]: unknown
}

export type DockerNetwork = {
  id: string
  name: string
  driver: string
  scope?: string
  internal?: boolean
  attachable?: boolean
  ingress?: boolean
  ipam?: Record<string, unknown>
  containers?: Record<string, unknown>
  labels?: Record<string, string>
  created?: string | number
  [key: string]: unknown
}

export type DockerVolume = {
  name: string
  driver: string
  mountpoint?: string
  scope?: string
  labels?: Record<string, string>
  options?: Record<string, string>
  created_at?: string | number
  usage_data?: { size?: number; ref_count?: number }
  [key: string]: unknown
}

export type DockerComposeProject = {
  name: string
  status?: string
  config_files?: string[]
  working_dir?: string
  services?: Array<string | { name: string; status?: string; replicas?: string }>
  containers?: number
  running?: number
  [key: string]: unknown
}

export type DockerComposeServiceSummary = {
  name: string
  image: string | null
  ports: string[]
  mounts: string[]
  networks: string[]
  profiles: string[]
}

export type DockerComposeConfigSummary = {
  service_count: number
  network_count: number
  volume_count: number
  config_count: number
  secret_count: number
}

export type DockerComposeValidation = {
  valid: boolean
  project_name: string | null
  services: string[]
  service_summaries: DockerComposeServiceSummary[]
  config_summary: DockerComposeConfigSummary
  warnings: string[]
  config_digest: string
  message?: string
  [key: string]: unknown
}

export type DockerDiskUsage = {
  layers_size?: number
  images?: Array<Record<string, unknown>>
  containers?: Array<Record<string, unknown>>
  volumes?: Array<Record<string, unknown>>
  build_cache?: Array<Record<string, unknown>>
  image_count?: number
  container_count?: number
  volume_count?: number
  images_size?: number
  containers_size?: number
  volumes_size?: number
  reclaimable_size?: number
  rows?: Array<Record<string, unknown>>
  [key: string]: unknown
}

export type DockerPruneStageResult = {
  resource: string
  completed: boolean
  message?: string
  output_truncated?: boolean
  error?: {
    code?: string
    message: string
    retryable?: boolean
    exit_code?: number | null
  }
  [key: string]: unknown
}

export type DockerOperationResult = {
  message?: string
  reclaimed_bytes?: number
  deleted?: string[]
  space_reclaimed?: number
  completed?: boolean
  partial_success?: boolean
  succeeded_stages?: number
  failed_stages?: number
  output_truncated?: boolean
  resources?: DockerPruneStageResult[]
  [key: string]: unknown
}

export type DockerContainerCreateInput = {
  name: string
  image: string
  command: string[]
  environment: string[]
  ports: Array<{
    container_port: number
    host_port: number | null
    host_ip: string | null
    protocol: 'tcp' | 'udp'
  }>
  volumes: Array<{ name: string; target: string; readonly: boolean }>
  bind_mounts: Array<{ source: string; target: string; readonly: boolean }>
  network: string | null
  restart_policy: 'no' | 'always' | 'unless-stopped' | 'on-failure'
  cpus: number | null
  memory_bytes: number | null
  confirm_read_write_bind_mounts: boolean
}

export type DockerComposeRequest = {
  project_name: string | null
  files: string[]
  profiles?: string[]
  services?: string[]
  confirm_risks?: boolean
  config_digest?: string
}

export type DockerLogServerMessage =
  | { type: 'opening' | 'ready'; cursor?: number | string | null }
  | { type: 'output' | 'line' | 'chunk'; data: string; encoding?: 'base64' | 'utf8'; ts?: number; cursor?: number | string }
  | { type: 'closed'; reason?: string | null; retryable?: boolean; cursor?: number | string | null }
  | { type: 'error'; code?: string; retryable?: boolean; message: string }

export type DockerTerminalClientMessage =
  | { type: 'input'; data: string }
  | { type: 'resize'; cols: number; rows: number }

export type DockerTerminalServerMessage =
  | { type: 'opening' }
  | { type: 'ready' }
  | { type: 'output'; data: string; encoding?: 'base64' | 'utf8' }
  | { type: 'closed'; exit_code?: number | null; reason?: string | null }
  | { type: 'error'; message: string }
