export type Metric = {
  ts: number
  cpu_percent: number
  memory_used: number
  memory_total: number
  disk_used: number
  disk_total: number
  network_rx: number
  network_tx: number
  gpu_percent: number | null
  gpu_memory_used: number | null
  gpu_memory_total: number | null
  uptime_seconds: number
  load_average: number | null
}

export type Instance = {
  id: string
  name: string
  region: string
  country_code: string
  country: string
  province_code: string
  province: string
  city: string
  remark: string
  hostname: string
  os: string
  arch: string
  agent_version: string
  online: boolean
  first_seen: number
  last_seen: number | null
  metrics: Metric | null
}

export type PendingInstance = {
  id: string
  hostname: string
  os: string
  arch: string
  agent_version: string
  first_seen: number
  last_seen: number
}

export type CommandRecord = {
  id: string
  name: string
  command: string
  confirm_text: string
  enabled: number
  created_at: number
}

export type CommandJob = {
  id: string
  command_id: string | null
  instance_id: string
  command: string
  status: string
  requested_by: string
  created_at: number
  completed_at: number | null
  output: string
  exit_code: number | null
}

export type ActionLog = {
  id: string
  actor: string
  action: string
  target: string
  detail: string
  created_at: number
}

export type SettingsResponse = {
  retention_days: number
  background_image_url: string | null
}

export type AppearanceResponse = {
  background_image_url: string | null
}

export type ViewMode = 'grid' | 'rows'

export type AdminTab = 'pending' | 'commands' | 'settings' | 'logs'

export type AppPage = 'home' | AdminTab
