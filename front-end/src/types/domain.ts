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

export type PublicDeviceGpuInfo = {
  name: string
  memory_total: number | null
}

export type PublicDeviceProfile = {
  schema_version: number
  collected_at: number
  os_name: string
  os_version: string
  architecture: string
  cpu_model: string
  physical_cores: number | null
  logical_cores: number
  memory_total: number
  storage_total: number
  gpus: PublicDeviceGpuInfo[]
}

export type PublicDeviceProfileResponse = {
  profile: PublicDeviceProfile | null
  updated_at: number | null
}

export type DeviceSystemInfo = {
  os_name: string
  os_version: string
  kernel_version: string
  architecture: string
}

export type DeviceCpuInfo = {
  model: string
  vendor: string
  physical_cores: number | null
  logical_cores: number
  frequency_mhz: number | null
}

export type DeviceGpuInfo = PublicDeviceGpuInfo & {
  vendor: string
}

export type DeviceDiskInfo = {
  name: string
  mount_point: string
  file_system: string
  kind: string
  total_bytes: number
}

export type DeviceNetworkInterface = {
  name: string
  mac_address: string | null
  ipv4: string[]
  ipv6: string[]
}

export type DeviceProfile = {
  schema_version: number
  collected_at: number
  system: DeviceSystemInfo
  cpu: DeviceCpuInfo
  memory_total: number
  storage_total: number
  gpus: DeviceGpuInfo[]
  disks: DeviceDiskInfo[]
  network_interfaces: DeviceNetworkInterface[]
}

export type AdminDeviceProfileResponse = {
  profile: DeviceProfile | null
  observed_ip: string | null
  updated_at: number | null
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

export type CommandExecutionState = {
  commandName: string
  instanceName: string
  job: CommandJob
  error: string
}

export type AuditEventStatus = 'running' | 'success' | 'partial_success' | 'failed' | 'cancelled'

export type AuditEvent = {
  id: string
  user_id: string | null
  actor: string
  category: string
  kind: string
  action: string
  target: string
  detail: string
  metadata: Record<string, unknown> | null
  instance_id: string | null
  node_snapshot: Record<string, unknown> | null
  source_ip: string | null
  user_agent: string | null
  request_id: string | null
  session_id: string | null
  operation_id: string | null
  status: AuditEventStatus
  error_code: string | null
  error_reason: string | null
  created_at: number
  completed_at: number | null
}

export type AuditQuery = {
  from: number | null
  to: number | null
  page: number
  page_size: number
  user_id: string
  actor: string
  category: string
  action: string
  instance_id: string
  status: AuditEventStatus | ''
  source_ip: string
  request_id: string
  keyword: string
}

export type AuditExportFormat = 'csv' | 'json'

export type AuditPage = {
  items: AuditEvent[]
  page: number
  page_size: number
  total: number
  pages: number
}

export type SettingsResponse = {
  retention_days: number
  audit_retention_days: number
  alert_retention_days: number
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

export type AgentRolloutState =
  | 'draft'
  | 'canary_active'
  | 'canary_paused'
  | 'full_active'
  | 'full_paused'
  | 'rollback_active'
  | 'rolled_back'
  | 'rollback_partial'

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
  status: AgentReleaseStatus
  published_at: number | null
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
  | 'cancelled'

export type AgentUpdateOperation = 'upgrade' | 'rollback'

export type AgentUpdateAttempt = {
  id: string
  release_id: string
  artifact_id: string | null
  instance_id: string
  operation: AgentUpdateOperation
  parent_attempt_id: string | null
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
  selected_instances: number
}

export type AgentRollbackCoverage = {
  succeeded_upgrades: number
  rollback_supported: number
  server_package_available: number
  local_package_available: number
  unavailable: number
  active_rollbacks: number
  failed_rollbacks: number
}

export type AgentRolloutCandidate = {
  instance_id: string
  name: string
  hostname: string
  os: string
  package_type: string
  native_arch: string
  agent_version: string
  online: boolean
  update_privileged: boolean
  selected: boolean
  eligible: boolean
  reason: string
  active_operation: AgentUpdateOperation | null
  active_status: AgentUpdateAttemptStatus | null
  rollback_supported: boolean
  rollback_version: string | null
}

export type AgentRelease = {
  id: string
  version: string
  notes: string
  status: AgentReleaseStatus
  rollout_state: AgentRolloutState
  rollout_updated_at: number | null
  created_at: number
  published_at: number | null
  artifacts: AgentArtifact[]
  attempts: AgentUpdateAttempt[]
  coverage: AgentReleaseCoverage
  rollback_coverage: AgentRollbackCoverage
}

export type AgentReleaseForm = {
  version: string
  notes: string
}

export type ViewMode = 'grid' | 'rows'

export type AdminTab = 'pending' | 'commands' | 'updates' | 'alerts' | 'users' | 'settings' | 'logs'

export type AppPage = 'home' | AdminTab

export type AlertMetric =
  | 'node_offline'
  | 'cpu_percent'
  | 'memory_percent'
  | 'disk_percent'
  | 'latency_ms'

export type AlertSeverity = 'warning' | 'critical'
export type AlertEventStatus = 'firing' | 'acknowledged' | 'resolved'
export type AlertRuleScope = 'all' | 'specific'
export type AlertMaintenanceScope = 'global' | 'rule' | 'node'
export type AlertDeliveryKind =
  | 'alert.firing'
  | 'alert.acknowledged'
  | 'alert.resolved'
  | 'webhook.test'
export type AlertDeliveryStatus = 'pending' | 'processing' | 'succeeded' | 'failed' | 'suppressed'
export type AlertCenterTab = 'events' | 'rules' | 'maintenance' | 'webhooks' | 'deliveries'

export type AlertPage<T> = {
  items: T[]
  page: number
  page_size: number
  total: number
  pages: number
}

export type AlertSummary = {
  firing: number
  acknowledged: number
  suppressed: number
  resolved_24h: number
}

export type AlertRule = {
  id: string
  name: string
  metric: AlertMetric
  threshold: number | null
  duration_seconds: number
  severity: AlertSeverity
  scope: AlertRuleScope
  enabled: boolean
  version: number
  created_by: string
  created_at: number
  updated_at: number
  target_instance_ids: string[]
  channel_ids: string[]
}

export type AlertRuleInput = Pick<
  AlertRule,
  'name' | 'metric' | 'threshold' | 'duration_seconds' | 'severity' | 'scope' | 'enabled'
> & {
  target_instance_ids: string[]
  channel_ids: string[]
}

export type AlertEventTimelineItem = {
  id: string
  event_id: string
  kind: string
  actor: string
  note: string
  value: number | null
  created_at: number
}

export type AlertEvent = {
  id: string
  rule_id: string
  instance_id: string
  status: AlertEventStatus
  severity: AlertSeverity
  metric: AlertMetric
  rule_snapshot: Record<string, unknown>
  node_snapshot: Record<string, unknown>
  threshold: number | null
  duration_seconds: number
  current_value: number | null
  first_observed_at: number
  fired_at: number
  last_observed_at: number
  match_count: number
  acknowledged_by: string | null
  acknowledged_by_user_id: string | null
  acknowledged_at: number | null
  acknowledge_note: string
  resolved_at: number | null
  resolution_reason: string
  suppressed: boolean
  suppression_reason: string
}

export type AlertEventDetail = AlertEvent & {
  timeline: AlertEventTimelineItem[]
  deliveries: AlertDelivery[]
}

export type AlertEventQuery = {
  page: number
  page_size: number
  status: AlertEventStatus | ''
  severity: AlertSeverity | ''
  metric: AlertMetric | ''
  instance_id: string
  suppressed: '' | 'true' | 'false'
  from: number | null
  to: number | null
  search: string
}

export type AlertMaintenanceWindow = {
  id: string
  name: string
  reason: string
  scope: AlertMaintenanceScope
  target_ids: string[]
  starts_at: number
  ends_at: number
  enabled: boolean
  created_by: string
  created_by_user_id: string | null
  created_at: number
  updated_at: number
}

export type AlertMaintenanceInput = Pick<
  AlertMaintenanceWindow,
  'name' | 'reason' | 'scope' | 'target_ids' | 'starts_at' | 'ends_at' | 'enabled'
>

// HTTP robot integrations use the same encrypted endpoint/secret contract in the backend.
// Keep the provider names explicit so the UI can render provider-specific guidance while
// preserving a stable channel type in rules and delivery snapshots.
export type AlertChannelType =
  | 'generic_webhook'
  | 'email'
  | 'feishu'
  | 'wecom'
  | 'dingtalk'
  | 'slack'
  | 'msteams'
  | 'telegram'
  | 'discord'
export type AlertEmailSecurity = 'starttls' | 'smtps'

export type AlertEmailChannelInput = {
  smtp_host?: string
  smtp_port?: number
  security?: AlertEmailSecurity
  username?: string
  password?: string
  clear_password: boolean
  from_address?: string
  from_name?: string
  recipients?: string[]
}

export type AlertNotificationChannel = {
  id: string
  name: string
  channel_type: AlertChannelType
  masked_url: string
  header_names: string[]
  has_secret: boolean
  smtp_host?: string | null
  smtp_port?: number | null
  security?: AlertEmailSecurity | null
  username?: string | null
  has_password: boolean
  from_address?: string | null
  from_name?: string | null
  recipients?: string[] | null
  chat_id?: string | null
  enabled: boolean
  created_at: number
  updated_at: number
}

export type AlertNotificationChannelInput = AlertEmailChannelInput & {
  name: string
  channel_type?: AlertChannelType
  url?: string
  secret?: string
  clear_secret: boolean
  headers?: Record<string, string>
  chat_id?: string
  enabled: boolean
}

// Keep compatibility aliases for consumers that still use the original names.
export type AlertWebhookChannel = AlertNotificationChannel
export type AlertWebhookChannelInput = AlertNotificationChannelInput

export type AlertDeliveryAttempt = {
  id: string
  delivery_id: string
  attempt_number: number
  http_status: number | null
  duration_ms: number
  error: string
  response_excerpt: string
  created_at: number
}

export type AlertDelivery = {
  id: string
  event_id: string | null
  channel_id: string
  kind: AlertDeliveryKind
  status: AlertDeliveryStatus
  payload: Record<string, unknown>
  channel_snapshot: Record<string, unknown>
  suppression_reason: string
  attempts_count: number
  cycle_attempts: number
  manual_retry_count: number
  next_attempt_at: number | null
  lease_until: number | null
  last_error: string
  created_at: number
  updated_at: number
  completed_at: number | null
}

export type AlertDeliveryDetail = AlertDelivery & {
  attempts: AlertDeliveryAttempt[]
}

export type AlertDeliveryQuery = {
  page: number
  page_size: number
  status: AlertDeliveryStatus | ''
  kind: AlertDeliveryKind | ''
  channel_id: string
  event_id: string
}

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
