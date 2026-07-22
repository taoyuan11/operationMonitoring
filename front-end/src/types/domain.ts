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
  latency_ms: number | null
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
  capabilities: string[]
  package_type?: string
  native_arch?: string
  update_privileged?: boolean
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
  package_type?: string
  native_arch?: string
  update_privileged?: boolean
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
  theme_mode: ThemeMode
  accent_color: string
}

export type AppearanceResponse = {
  background_image_url: string | null
  theme_mode: ThemeMode
  accent_color: string
}

export type ThemeMode = 'auto' | 'light' | 'dark'
export type ResolvedTheme = 'light' | 'dark'

export type AuthMode = 'bootstrap' | 'totp'

export type SessionUser = {
  id: string
  username: string
}

export type AuthEnrollment = {
  id: string
  username: string
  device_name: string
  otpauth_uri: string
  expires_at: number
}

export type AuthenticatorDevice = {
  id: string
  name: string
  created_at: number
  last_used_at: number | null
}

export type AdminUser = SessionUser & {
  enabled: boolean
  created_at: number
  devices: AuthenticatorDevice[]
}

export type PendingAuthEnrollment = {
  id: string
  target_user_id: string | null
  username: string
  device_name: string
  created_at: number
  expires_at: number
}

export type AdminUsersResponse = {
  users: AdminUser[]
  enrollments: PendingAuthEnrollment[]
}

export type AgentReleaseStatus = 'draft' | 'published'

export type AgentPackageType = 'standalone'

export type AgentArtifactTarget = {
  os: string
  package_type: AgentPackageType
  native_arch: string
}

export type AgentArtifactUploadRow = AgentArtifactTarget & {
  id: string
  file: File | null
  checksum_file: File | null
  error: string
  inference: 'manual' | 'matched' | 'needs_target' | 'needs_architecture'
}

export type AgentArtifactUploadItem = {
  row_id: string
  target: AgentArtifactTarget
  file: File
  checksum_file: File
}

export type AgentArtifactUploadResult = {
  succeeded_row_ids: string[]
  failures: Array<{ row_id: string; message: string }>
}

export type AgentArtifact = AgentArtifactTarget & {
  id: string
  release_id: string
  file_name: string
  size_bytes: number
  sha256: string
  created_at: number
}

export type AgentUpdateAttemptStatus =
  | 'pending'
  | 'waiting'
  | 'downloading'
  | 'verifying'
  | 'waiting_idle'
  | 'installing'
  | 'awaiting_restart'
  | 'succeeded'
  | 'rollback_succeeded'
  | 'failed'

export type AgentUpdateAttempt = {
  id: string
  release_id: string
  artifact_id: string
  instance_id: string
  from_version: string
  target_version: string
  status: AgentUpdateAttemptStatus
  message: string
  retry_count: number
  created_at: number
  updated_at: number
  completed_at: number | null
}

export type AgentReleaseCoverage = {
  eligible_instances: number
  covered_instances: number
  missing_artifact_instances: number
  unprivileged_instances: number
}

export type AgentRelease = {
  id: string
  version: string
  notes: string
  status: AgentReleaseStatus
  created_at: number
  published_at: number | null
  artifacts: AgentArtifact[]
  attempts: AgentUpdateAttempt[]
  coverage: AgentReleaseCoverage
}

export type AgentReleaseForm = {
  version: string
  notes: string
}

export type ViewMode = 'grid' | 'rows'

export type AdminTab = 'pending' | 'commands' | 'updates' | 'users' | 'settings' | 'logs'

export type AppPage = 'home' | AdminTab

export type FileEntryKind = 'file' | 'directory' | 'symlink' | 'other'

export type InstanceFileRoot = {
  path: string
  label: string
}

export type InstanceFileEntry = {
  name: string
  path: string
  kind: FileEntryKind
  size_bytes: number
  modified_at: number | null
  readonly: boolean
}

export type InstanceFileListing = {
  path: string
  parent: string | null
  entries: InstanceFileEntry[]
  offset: number
  limit: number
  total: number
}

export type FileRootsResponse = {
  roots: InstanceFileRoot[]
  max_file_bytes: number
}

export type FileOperationResult = {
  path: string
}
