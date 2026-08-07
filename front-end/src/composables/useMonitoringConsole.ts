import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import { ApiError, api as rawApi } from '../api/http'
import { auditExportFileName, auditQueryPath, downloadAuditExport } from '../api/audit'
import { getCountryOption } from '../data/countries'
import { useAppearance } from './useAppearance'
import type {
  AgentArtifactTarget,
  AgentArtifactUploadItem,
  AgentArtifactUploadResult,
  AgentRelease,
  AgentReleaseForm,
  AgentRolloutCandidate,
  AgentUpdateAttempt,
  AdminUser,
  AdminUsersResponse,
  AdminTab,
  AppearanceResponse,
  AuditExportFormat,
  AuditPage,
  AuditQuery,
  AuthEnrollment,
  AuthMode,
  CommandExecutionState,
  CommandJob,
  CommandRecord,
  Instance,
  PendingInstance,
  PendingAuthEnrollment,
  SessionUser,
  SettingsResponse,
  ViewMode,
} from '../types/domain'
import { average, formatDateTimeInput, parseDateTimeInput } from '../utils/format'

type TrafficSnapshot = {
  counters: Map<string, { rx: number; tx: number }>
  capturedAt: number
}

type AgentUpdateOperation =
  | 'creating'
  | 'saving'
  | 'uploading'
  | 'publishing'
  | 'deleting'
  | 'retrying'
  | 'targeting'
  | 'pausing'
  | 'resuming'
  | 'promoting'
  | 'rolling_back'
  | 'reupgrading'
  | null

const COMMAND_JOB_POLL_INTERVAL_MS = 750
const COMMAND_JOB_POLL_RETRY_MS = 2000
const DEFAULT_AUDIT_PAGE_SIZE = 50

function defaultAuditQuery(): AuditQuery {
  return {
    from: null,
    to: null,
    page: 1,
    page_size: DEFAULT_AUDIT_PAGE_SIZE,
    user_id: '',
    actor: '',
    category: '',
    action: '',
    instance_id: '',
    status: '',
    source_ip: '',
    request_id: '',
    keyword: '',
  }
}

function emptyAuditPage(): AuditPage {
  return { items: [], page: 1, page_size: DEFAULT_AUDIT_PAGE_SIZE, total: 0, pages: 0 }
}

function abortedRequestError() {
  return new DOMException('Request superseded by a session change', 'AbortError')
}

function isAbortError(error: unknown) {
  return error instanceof Error && error.name === 'AbortError'
}

export function useMonitoringConsole() {
  const appearance = useAppearance()
  const instances = ref<Instance[]>([])
  const pendingInstances = ref<PendingInstance[]>([])
  const commands = ref<CommandRecord[]>([])
  const jobs = ref<CommandJob[]>([])
  const commandExecution = ref<CommandExecutionState | null>(null)
  const audit = ref<AuditPage>(emptyAuditPage())
  const auditQuery = reactive<AuditQuery>(defaultAuditQuery())
  const auditLoading = ref(false)
  const auditError = ref('')
  const auditExporting = ref<AuditExportFormat | null>(null)
  const agentReleases = ref<AgentRelease[]>([])
  const agentUpdateAttempts = ref<AgentUpdateAttempt[]>([])
  const agentRolloutCandidates = ref<Record<string, AgentRolloutCandidate[]>>({})
  const agentRolloutCandidatesLoading = ref('')
  const adminUsers = ref<AdminUser[]>([])
  const authEnrollments = ref<PendingAuthEnrollment[]>([])
  const activeAuthEnrollment = ref<AuthEnrollment | null>(null)
  const authMode = ref<AuthMode>('totp')
  const currentUser = ref<SessionUser | null>(null)
  const isAdmin = ref(false)
  const sessionReady = ref(false)
  const publicReady = ref(false)
  const activeTaskCount = ref(0)
  const loading = computed(() => activeTaskCount.value > 0)
  const refreshing = ref(false)
  const errorMessage = ref('')
  const viewMode = ref<ViewMode>('grid')
  const adminTab = ref<AdminTab>('pending')
  const currentTime = ref(new Date())
  const backgroundFileName = ref('')
  const backgroundOperation = ref<'uploading' | 'removing' | null>(null)
  const backgroundMessage = ref('')
  const appearanceMessage = ref('')
  const agentUpdateOperation = ref<AgentUpdateOperation>(null)
  const agentUpdateBusyId = ref('')
  const agentUpdateMessage = ref('')
  const adminResetKey = ref(0)
  const trafficSnapshot = ref<TrafficSnapshot | null>(null)
  const networkRxRate = ref(0)
  const networkTxRate = ref(0)
  let publicPollTimer: number | null = null
  let clockTimer: number | null = null
  let publicRequestInFlight = false
  let guardedRequest = 0
  let publicDataRequest = 0
  let appearanceRequest = 0
  let sessionRequest = 0
  let adminDataRequest = 0
  let auditDataRequest = 0
  let auditExportRequest = 0
  let agentUpdatesRequest = 0
  let usersRequest = 0
  let sessionEpoch = 0
  const trackedCommandJobs = new Set<string>()
  const commandJobPollTimers = new Map<string, number>()
  let adminAbortController = new AbortController()
  const operationLocks = new Set<string>()

  const loginForm = reactive({
    username: '',
    password: '',
    code: '',
  })

  const userAuthForm = reactive({
    username: '',
    current_code: '',
    confirmation_code: '',
  })

  const settingsForm = reactive({
    retention_days: 30,
    audit_retention_days: 180,
    alert_retention_days: 180,
    background_image_url: null as string | null,
    theme_mode: appearance.themeMode.value,
    accent_color: appearance.accentColor.value,
  })

  const commandForm = reactive({
    name: '',
    command: '',
    confirm_text: '',
  })

  const agentReleaseForm = reactive<AgentReleaseForm>({
    version: '',
    notes: '',
  })

  const editInstance = ref<Instance | null>(null)
  const editForm = reactive({
    name: '',
    country_code: '',
    country: '',
    remark: '',
    expires_at: '',
  })

  const terminalState = reactive<{
    instance: Instance | null
  }>({
    instance: null,
  })

  const remoteDesktopState = reactive<{
    instance: Instance | null
  }>({
    instance: null,
  })

  const onlineCount = computed(() => instances.value.filter((item) => item.online).length)
  const avgCpu = computed(() =>
    average(instances.value.filter((item) => item.online).map((item) => item.metrics?.cpu_percent)),
  )
  const avgMemory = computed(() =>
    average(
      instances.value.filter((item) => item.online).map((item) =>
        item.metrics && item.metrics.memory_total > 0
          ? (item.metrics.memory_used / item.metrics.memory_total) * 100
          : null,
      ),
    ),
  )
  const totalNetworkRx = computed(() =>
    instances.value.reduce((sum, item) => sum + (item.metrics?.network_rx || 0), 0),
  )
  const totalNetworkTx = computed(() =>
    instances.value.reduce((sum, item) => sum + (item.metrics?.network_tx || 0), 0),
  )
  const totalTraffic = computed(() => totalNetworkRx.value + totalNetworkTx.value)
  onMounted(async () => {
    const initialResults = await Promise.allSettled([loadAppearance(), loadPublic(), checkSession()])
    const publicResult = initialResults[1]
    if (publicResult.status === 'rejected') {
      errorMessage.value = publicResult.reason instanceof Error
        ? publicResult.reason.message
        : '暂时无法加载监控数据'
    }
    sessionReady.value = true
    publicReady.value = true

    publicPollTimer = window.setInterval(pollPublic, 5000)
    clockTimer = window.setInterval(() => {
      currentTime.value = new Date()
    }, 1000)
  })

  onBeforeUnmount(() => {
    if (publicPollTimer !== null) window.clearInterval(publicPollTimer)
    if (clockTimer !== null) window.clearInterval(clockTimer)
    clearTrackedCommandJobs()
    invalidateAdminRequests()
  })

  watch(
    [() => adminTab.value, () => isAdmin.value],
    ([tab, admin]) => {
      if (tab === 'logs' && admin) void loadAudit()
    },
  )

  async function guarded(task: () => Promise<void>, operationKey?: string): Promise<boolean> {
    if (operationKey && operationLocks.has(operationKey)) return false
    if (operationKey) operationLocks.add(operationKey)
    const request = ++guardedRequest
    activeTaskCount.value += 1
    errorMessage.value = ''
    try {
      await task()
      return true
    } catch (error) {
      if (request === guardedRequest && !isAbortError(error)) {
        errorMessage.value = error instanceof Error ? error.message : '操作失败'
      }
      return false
    } finally {
      activeTaskCount.value = Math.max(0, activeTaskCount.value - 1)
      if (operationKey) operationLocks.delete(operationKey)
    }
  }

  async function api<T>(path: string, options: RequestInit = {}): Promise<T> {
    if (!path.startsWith('/api/admin')) return rawApi<T>(path, options)

    const epoch = sessionEpoch
    const controller = adminAbortController
    let response: T
    try {
      response = await rawApi<T>(path, {
        ...options,
        signal: controller.signal,
      })
    } catch (error) {
      if (
        error instanceof ApiError
        && error.status === 401
        && isAdmin.value
        && epoch === sessionEpoch
        && !controller.signal.aborted
      ) {
        const session = await rawApi<{ authenticated: boolean }>('/api/admin/me', {
          signal: controller.signal,
        }).catch(() => null)
        if (
          session?.authenticated === false
          && epoch === sessionEpoch
          && !controller.signal.aborted
        ) {
          invalidateAdminRequests()
          clearAdminState()
        }
      }
      throw error
    }
    if (epoch !== sessionEpoch || controller.signal.aborted) throw abortedRequestError()
    return response
  }

  function invalidateAdminRequests() {
    adminAbortController.abort()
    sessionEpoch += 1
    adminAbortController = new AbortController()
  }

  function clearAdminState() {
    clearTrackedCommandJobs()
    terminalState.instance = null
    remoteDesktopState.instance = null
    editInstance.value = null
    isAdmin.value = false
    currentUser.value = null
    pendingInstances.value = []
    commands.value = []
    jobs.value = []
    commandExecution.value = null
    auditDataRequest += 1
    auditExportRequest += 1
    audit.value = emptyAuditPage()
    Object.assign(auditQuery, defaultAuditQuery())
    auditLoading.value = false
    auditError.value = ''
    auditExporting.value = null
    agentReleases.value = []
    agentUpdateAttempts.value = []
    adminUsers.value = []
    authEnrollments.value = []
    activeAuthEnrollment.value = null
    loginForm.username = ''
    loginForm.password = ''
    loginForm.code = ''
    userAuthForm.username = ''
    userAuthForm.current_code = ''
    userAuthForm.confirmation_code = ''
    commandForm.name = ''
    commandForm.command = ''
    commandForm.confirm_text = ''
    agentReleaseForm.version = ''
    agentReleaseForm.notes = ''
    editForm.name = ''
    editForm.country_code = ''
    editForm.country = ''
    editForm.remark = ''
    editForm.expires_at = ''
    settingsForm.retention_days = 30
    settingsForm.audit_retention_days = 180
    settingsForm.alert_retention_days = 180
    settingsForm.background_image_url = appearance.backgroundImageUrl.value
    settingsForm.theme_mode = appearance.themeMode.value
    settingsForm.accent_color = appearance.accentColor.value
    adminTab.value = 'pending'
    backgroundFileName.value = ''
    backgroundOperation.value = null
    backgroundMessage.value = ''
    appearanceMessage.value = ''
    agentUpdateOperation.value = null
    agentUpdateBusyId.value = ''
    agentUpdateMessage.value = ''
    adminResetKey.value += 1
  }

  async function loadPublic() {
    const request = ++publicDataRequest
    const nextInstances = await api<Instance[]>('/api/public/instances')
    if (request !== publicDataRequest) return
    updateNetworkRates(nextInstances)
    instances.value = nextInstances
  }

  async function pollPublic() {
    if (publicRequestInFlight) return
    publicRequestInFlight = true
    try {
      await Promise.all([
        loadPublic(),
        isAdmin.value && !agentUpdateOperation.value ? loadAgentUpdates() : Promise.resolve(),
      ])
    } catch {
      // Keep the last valid snapshot during transient network failures.
    } finally {
      publicRequestInFlight = false
    }
  }

  async function loadAppearance() {
    const request = ++appearanceRequest
    const response = await api<AppearanceResponse>('/api/public/appearance')
    if (request !== appearanceRequest) return
    appearance.applyAppearance(response)
  }

  async function checkSession() {
    const request = ++sessionRequest
    const [status, me] = await Promise.all([
      api<{ mode: AuthMode }>('/api/admin/auth/status'),
      api<{ authenticated: boolean; user: SessionUser | null }>('/api/admin/me'),
    ])
    if (request !== sessionRequest) return
    authMode.value = status.mode
    isAdmin.value = me.authenticated
    currentUser.value = me.user
    if (me.authenticated) {
      await loadAdminData()
    } else {
      invalidateAdminRequests()
      clearAdminState()
    }
  }

  async function loadAdminData() {
    const request = ++adminDataRequest
    const userRequest = ++usersRequest
    const [pending, commandList, jobList, settings, users] = await Promise.all([
      api<PendingInstance[]>('/api/admin/pending-instances'),
      api<CommandRecord[]>('/api/admin/commands'),
      api<CommandJob[]>('/api/admin/jobs'),
      api<SettingsResponse>('/api/admin/settings'),
      api<AdminUsersResponse>('/api/admin/users'),
      loadAgentUpdates(),
    ])
    if (request !== adminDataRequest) return
    pendingInstances.value = pending
    commands.value = commandList
    jobs.value = jobList
    settingsForm.retention_days = settings.retention_days
    settingsForm.audit_retention_days = settings.audit_retention_days ?? 180
    settingsForm.alert_retention_days = settings.alert_retention_days ?? 180
    appearance.applyAppearance(settings)
    settingsForm.background_image_url = settings.background_image_url
    settingsForm.theme_mode = settings.theme_mode
    settingsForm.accent_color = settings.accent_color
    if (userRequest === usersRequest) applyUsers(users)
    if (adminTab.value === 'logs') void loadAudit()
  }

  async function loadAudit() {
    if (!isAdmin.value) return
    const request = ++auditDataRequest
    auditLoading.value = true
    auditError.value = ''
    try {
      const response = await api<AuditPage>(auditQueryPath(auditQuery))
      if (request !== auditDataRequest) return
      audit.value = {
        items: response.items || [],
        page: response.page || auditQuery.page,
        page_size: response.page_size || auditQuery.page_size,
        total: response.total || 0,
        pages: response.pages || 0,
      }
      if (audit.value.pages > 0 && audit.value.page > audit.value.pages) {
        auditQuery.page = audit.value.pages
        void loadAudit()
        return
      }
      auditQuery.page = audit.value.page
      auditQuery.page_size = audit.value.page_size
    } catch (error) {
      if (request !== auditDataRequest || isAbortError(error)) return
      auditError.value = error instanceof Error ? error.message : '审计记录加载失败'
    } finally {
      if (request === auditDataRequest) auditLoading.value = false
    }
  }

  function updateAuditQuery(patch: Partial<AuditQuery>) {
    Object.assign(auditQuery, patch)
    auditQuery.page = 1
    void loadAudit()
  }

  function setAuditPage(page: number) {
    const nextPage = Math.max(1, Math.min(page, audit.value.pages || 1))
    if (nextPage === auditQuery.page) return
    auditQuery.page = nextPage
    void loadAudit()
  }

  async function exportAudit(format: AuditExportFormat) {
    if (auditExporting.value) return
    const request = ++auditExportRequest
    const epoch = sessionEpoch
    const controller = adminAbortController
    auditExporting.value = format
    auditError.value = ''
    try {
      const blob = await downloadAuditExport(auditQuery, format, controller.signal)
      if (request !== auditExportRequest || epoch !== sessionEpoch || controller.signal.aborted) {
        throw abortedRequestError()
      }
      const url = URL.createObjectURL(blob)
      const anchor = document.createElement('a')
      anchor.href = url
      anchor.download = auditExportFileName(format)
      document.body.appendChild(anchor)
      anchor.click()
      anchor.remove()
      window.setTimeout(() => URL.revokeObjectURL(url), 0)
    } catch (error) {
      if (request !== auditExportRequest || isAbortError(error)) return
      auditError.value = error instanceof Error ? error.message : '审计记录导出失败'
    } finally {
      if (request === auditExportRequest) auditExporting.value = null
    }
  }

  async function loadAgentUpdates() {
    const request = ++agentUpdatesRequest
    const [releases, attempts] = await Promise.all([
      api<AgentRelease[]>('/api/admin/agent-releases'),
      api<AgentUpdateAttempt[]>('/api/admin/agent-update-attempts'),
    ])
    if (request !== agentUpdatesRequest) return
    agentUpdateAttempts.value = attempts
    agentReleases.value = releases.map((release) => ({
      ...release,
      artifacts: release.artifacts || [],
      attempts: release.attempts?.length
        ? release.attempts
        : attempts.filter((attempt) => attempt.release_id === release.id),
      coverage: release.coverage || {
        eligible_instances: 0,
        covered_instances: 0,
        missing_artifact_instances: 0,
        unprivileged_instances: 0,
        selected_instances: 0,
      },
      rollback_coverage: release.rollback_coverage || {
        succeeded_upgrades: 0,
        rollback_supported: 0,
        server_package_available: 0,
        local_package_available: 0,
        unavailable: 0,
        active_rollbacks: 0,
        failed_rollbacks: 0,
      },
    }))
  }

  async function loadAgentRolloutCandidates(releaseId: string) {
    if (agentRolloutCandidatesLoading.value === releaseId) return false
    agentRolloutCandidatesLoading.value = releaseId
    const success = await guarded(async () => {
      agentRolloutCandidates.value = {
        ...agentRolloutCandidates.value,
        [releaseId]: await api<AgentRolloutCandidate[]>(
          `/api/admin/agent-releases/${releaseId}/rollout/candidates`,
        ),
      }
    })
    if (agentRolloutCandidatesLoading.value === releaseId) {
      agentRolloutCandidatesLoading.value = ''
    }
    return success
  }

  function refreshAll() {
    if (refreshing.value) return
    refreshing.value = true
    void guarded(async () => {
      await Promise.all([loadPublic(), loadAppearance()])
      if (isAdmin.value) {
        await loadAdminData()
      }
    }).finally(() => {
      refreshing.value = false
    })
  }

  function login() {
    guarded(async () => {
      if (authMode.value === 'bootstrap' && !activeAuthEnrollment.value) {
        activeAuthEnrollment.value = await api<AuthEnrollment>('/api/admin/bootstrap/start', {
          method: 'POST',
          body: JSON.stringify({
            username: loginForm.username,
            password: loginForm.password,
          }),
        })
        loginForm.password = ''
        loginForm.code = ''
        return
      }

      const response = authMode.value === 'bootstrap' && activeAuthEnrollment.value
        ? await api<{ role: 'admin'; user: SessionUser }>(
            `/api/admin/bootstrap/enrollments/${activeAuthEnrollment.value.id}/confirm`,
            {
              method: 'POST',
              body: JSON.stringify({ code: loginForm.code }),
            },
          )
        : await api<{ role: 'admin'; user: SessionUser }>('/api/admin/login', {
            method: 'POST',
            body: JSON.stringify({ username: loginForm.username, code: loginForm.code }),
          })
      invalidateAdminRequests()
      isAdmin.value = true
      currentUser.value = response.user
      authMode.value = 'totp'
      activeAuthEnrollment.value = null
      loginForm.password = ''
      loginForm.code = ''
      await loadAdminData()
    }, 'auth:login')
  }

  function restartBootstrap() {
    activeAuthEnrollment.value = null
    loginForm.code = ''
    errorMessage.value = ''
  }

  function logout() {
    return guarded(async () => {
      await rawApi('/api/admin/logout', { method: 'POST' })
      invalidateAdminRequests()
      clearAdminState()
    }, 'auth:logout')
  }

  function approveInstance(id: string) {
    guarded(async () => {
      await api(`/api/admin/pending-instances/${id}/approve`, { method: 'POST' })
      await Promise.all([loadPublic(), loadAdminData()])
    }, `pending:${id}`)
  }

  function rejectInstance(id: string) {
    guarded(async () => {
      await api(`/api/admin/pending-instances/${id}/reject`, { method: 'POST' })
      await loadAdminData()
    }, `pending:${id}`)
  }

  function openEdit(instance: Instance) {
    const country = getCountryOption(instance.country_code)
    editInstance.value = instance
    editForm.name = instance.name
    editForm.country_code = country?.code || ''
    editForm.country = country?.name || ''
    editForm.remark = instance.remark
    editForm.expires_at = formatDateTimeInput(instance.expires_at)
  }

  function closeEdit() {
    editInstance.value = null
  }

  function saveEdit() {
    if (!editInstance.value) return
    const id = editInstance.value.id
    guarded(async () => {
      await api(`/api/admin/instances/${id}`, {
        method: 'PATCH',
        body: JSON.stringify({
          name: editForm.name,
          country_code: editForm.country_code,
          country: editForm.country,
          remark: editForm.remark,
          expires_at: parseDateTimeInput(editForm.expires_at),
          province_code: '',
          province: '',
          city: '',
        }),
      })
      editInstance.value = null
      await loadPublic()
    }, `instance:${id}`)
  }

  function disableInstance(instance: Instance) {
    guarded(async () => {
      await api(`/api/admin/instances/${instance.id}/disable`, { method: 'POST' })
      await loadPublic()
    }, `instance:${instance.id}`)
  }

  function deleteInstance(instance: Instance) {
    guarded(async () => {
      await api(`/api/admin/instances/${instance.id}`, { method: 'DELETE' })
      await Promise.all([loadPublic(), loadAdminData()])
    }, `instance:${instance.id}`)
  }

  function createCommand() {
    guarded(async () => {
      await api('/api/admin/commands', {
        method: 'POST',
        body: JSON.stringify(commandForm),
      })
      commandForm.name = ''
      commandForm.command = ''
      commandForm.confirm_text = ''
      await loadAdminData()
    }, 'command:create')
  }

  function removeCommand(command: CommandRecord) {
    guarded(async () => {
      await api(`/api/admin/commands/${command.id}`, { method: 'DELETE' })
      await loadAdminData()
    }, `command:${command.id}`)
  }

  function runCommand(instance: Instance, command: CommandRecord) {
    guarded(async () => {
      const job = await api<CommandJob>(`/api/admin/instances/${instance.id}/commands/${command.id}/run`, {
        method: 'POST',
      })
      upsertCommandJob(job)
      commandExecution.value = {
        commandName: command.name,
        instanceName: instance.name || instance.hostname,
        job,
        error: '',
      }
      trackCommandJob(job.id)
    }, `run-command:${instance.id}:${command.id}`)
  }

  function closeCommandExecution() {
    commandExecution.value = null
  }

  function trackCommandJob(jobId: string) {
    if (trackedCommandJobs.has(jobId)) return
    trackedCommandJobs.add(jobId)
    void pollCommandJob(jobId)
  }

  async function pollCommandJob(jobId: string) {
    if (!trackedCommandJobs.has(jobId)) return
    commandJobPollTimers.delete(jobId)
    try {
      const job = await api<CommandJob>(`/api/admin/jobs/${encodeURIComponent(jobId)}`, {
        cache: 'no-store',
      })
      if (!trackedCommandJobs.has(jobId)) return
      upsertCommandJob(job)
      if (commandExecution.value?.job.id === jobId) {
        commandExecution.value = {
          ...commandExecution.value,
          job,
          error: '',
        }
      }
      if (isTerminalCommandJob(job)) {
        trackedCommandJobs.delete(jobId)
        return
      }
      scheduleCommandJobPoll(jobId, COMMAND_JOB_POLL_INTERVAL_MS)
    } catch (error) {
      if (!trackedCommandJobs.has(jobId) || isAbortError(error)) return
      if (commandExecution.value?.job.id === jobId) {
        commandExecution.value = {
          ...commandExecution.value,
          error: error instanceof Error ? error.message : '暂时无法获取命令结果',
        }
      }
      if (error instanceof ApiError && error.status === 404) {
        trackedCommandJobs.delete(jobId)
        return
      }
      scheduleCommandJobPoll(jobId, COMMAND_JOB_POLL_RETRY_MS)
    }
  }

  function scheduleCommandJobPoll(jobId: string, delay: number) {
    if (!trackedCommandJobs.has(jobId)) return
    const timer = window.setTimeout(() => {
      commandJobPollTimers.delete(jobId)
      void pollCommandJob(jobId)
    }, delay)
    commandJobPollTimers.set(jobId, timer)
  }

  function upsertCommandJob(job: CommandJob) {
    const existingIndex = jobs.value.findIndex((candidate) => candidate.id === job.id)
    if (existingIndex === -1) {
      jobs.value = [job, ...jobs.value].slice(0, 200)
      return
    }
    jobs.value = jobs.value.map((candidate, index) => index === existingIndex ? job : candidate)
  }

  function clearTrackedCommandJobs() {
    trackedCommandJobs.clear()
    for (const timer of commandJobPollTimers.values()) window.clearTimeout(timer)
    commandJobPollTimers.clear()
  }

  function isTerminalCommandJob(job: CommandJob) {
    return job.status === 'completed' || job.status === 'failed'
  }

  function saveSettings() {
    guarded(async () => {
      await api('/api/admin/settings', {
        method: 'PUT',
        body: JSON.stringify({
          retention_days: settingsForm.retention_days,
          audit_retention_days: settingsForm.audit_retention_days,
          alert_retention_days: settingsForm.alert_retention_days,
        }),
      })
      await loadAdminData()
    }, 'settings:retention')
  }

  function saveAppearance() {
    appearanceMessage.value = ''
    return guarded(async () => {
      const settings = await api<SettingsResponse>('/api/admin/settings/appearance', {
        method: 'PUT',
        body: JSON.stringify({
          theme_mode: settingsForm.theme_mode,
          accent_color: settingsForm.accent_color,
        }),
      })
      appearance.applyAppearance(settings)
      settingsForm.theme_mode = settings.theme_mode
      settingsForm.accent_color = settings.accent_color
      appearanceMessage.value = '外观设置已保存并应用'
    }, 'settings:appearance')
  }

  function createUserEnrollment() {
    return runUserAuthTask(async () => {
      activeAuthEnrollment.value = await api<AuthEnrollment>('/api/admin/users/enrollments', {
        method: 'POST',
        body: JSON.stringify({
          username: userAuthForm.username,
          current_code: userAuthForm.current_code,
        }),
      })
      userAuthForm.username = ''
      userAuthForm.confirmation_code = ''
      await loadUsers()
    })
  }

  function createDeviceEnrollment(user: AdminUser) {
    return runUserAuthTask(async () => {
      activeAuthEnrollment.value = await api<AuthEnrollment>(
        `/api/admin/users/${user.id}/device-enrollments`,
        {
          method: 'POST',
          body: JSON.stringify({ current_code: userAuthForm.current_code }),
        },
      )
      userAuthForm.confirmation_code = ''
      await loadUsers()
    })
  }

  function confirmAuthEnrollment() {
    if (!activeAuthEnrollment.value) return Promise.resolve(false)
    const enrollmentId = activeAuthEnrollment.value.id
    return runUserAuthTask(async () => {
      const users = await api<AdminUsersResponse>(
        `/api/admin/auth/enrollments/${enrollmentId}/confirm`,
        {
          method: 'POST',
          body: JSON.stringify({ code: userAuthForm.confirmation_code }),
        },
      )
      activeAuthEnrollment.value = null
      userAuthForm.confirmation_code = ''
      applyUsers(users)
    }, false)
  }

  function cancelAuthEnrollment(enrollmentId: string) {
    return runUserAuthTask(async () => {
      await api(`/api/admin/auth/enrollments/${enrollmentId}`, {
        method: 'DELETE',
        body: JSON.stringify({ current_code: userAuthForm.current_code }),
      })
      if (activeAuthEnrollment.value?.id === enrollmentId) activeAuthEnrollment.value = null
      await loadUsers()
    })
  }

  function setAdminUserEnabled(user: AdminUser, enabled: boolean) {
    return runUserAuthTask(async () => {
      await api(`/api/admin/users/${user.id}/enabled`, {
        method: 'PATCH',
        body: JSON.stringify({ enabled, current_code: userAuthForm.current_code }),
      })
      await loadUsers()
    })
  }

  function deleteAdminUser(user: AdminUser) {
    return runUserAuthTask(async () => {
      await api(`/api/admin/users/${user.id}`, {
        method: 'DELETE',
        body: JSON.stringify({ current_code: userAuthForm.current_code }),
      })
      await loadUsers()
    })
  }

  function revokeAuthenticatorDevice(deviceId: string) {
    return runUserAuthTask(async () => {
      await api(`/api/admin/auth/devices/${deviceId}`, {
        method: 'DELETE',
        body: JSON.stringify({ current_code: userAuthForm.current_code }),
      })
      await checkSession()
    })
  }

  async function runUserAuthTask(task: () => Promise<void>, clearCurrentCode = true) {
    const success = await guarded(task)
    if (clearCurrentCode) userAuthForm.current_code = ''
    return success
  }

  async function loadUsers() {
    const request = ++usersRequest
    const users = await api<AdminUsersResponse>('/api/admin/users')
    if (request === usersRequest) applyUsers(users)
  }

  function applyUsers(response: AdminUsersResponse) {
    adminUsers.value = response.users
    authEnrollments.value = response.enrollments
  }

  async function createAgentRelease(onCreated?: (releaseId: string) => void) {
    const form = {
      version: agentReleaseForm.version.trim(),
      notes: agentReleaseForm.notes.trim(),
    }
    if (!form.version) {
      errorMessage.value = '请输入 Agent 版本号'
      return false
    }

    let createdReleaseId = ''
    const success = await runAgentUpdateTask('creating', 'new-release', async () => {
      const release = await api<AgentRelease>('/api/admin/agent-releases', {
        method: 'POST',
        body: JSON.stringify(form),
      })
      createdReleaseId = release.id
      agentReleaseForm.version = ''
      agentReleaseForm.notes = ''
      await loadAgentUpdates()
    }, '已创建更新草稿')
    if (success && createdReleaseId) onCreated?.(createdReleaseId)
    return success
  }

  function saveAgentRelease(releaseId: string, form: AgentReleaseForm) {
    const payload = {
      version: form.version.trim(),
      notes: form.notes.trim(),
    }
    if (!payload.version) {
      errorMessage.value = '请输入 Agent 版本号'
      return Promise.resolve(false)
    }

    return runAgentUpdateTask('saving', releaseId, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${releaseId}`, {
        method: 'PATCH',
        body: JSON.stringify(payload),
      })
      await loadAgentUpdates()
    }, '草稿已保存')
  }

  async function uploadAgentArtifact(
    releaseId: string,
    uploads: AgentArtifactUploadItem[],
    onComplete: (result: AgentArtifactUploadResult) => void,
  ) {
    if (!uploads.length || agentUpdateOperation.value) return false

    const result: AgentArtifactUploadResult = {
      succeeded_row_ids: [],
      failures: [],
    }
    agentUpdateMessage.value = ''
    agentUpdateOperation.value = 'uploading'
    agentUpdateBusyId.value = releaseId

    const completed = await guarded(async () => {
      for (const upload of uploads) {
        const normalizedTarget: AgentArtifactTarget = {
          ...upload.target,
          os: upload.target.os.trim(),
          native_arch: upload.target.native_arch.trim(),
        }
        let validationError = ''
        if (!normalizedTarget.os || !normalizedTarget.native_arch) {
          validationError = '请选择目标系统和原生架构'
        } else if (upload.file.size === 0) {
          validationError = '可执行文件不能为空'
        } else if (upload.checksum_file.size === 0) {
          validationError = 'SHA-256 校验文件不能为空'
        } else if (normalizedTarget.package_type !== 'standalone') {
          validationError = '仅支持 standalone 可执行文件'
        } else {
          const expectedExtension = normalizedTarget.os === 'windows' ? 'exe' : 'bin'
          if (!upload.file.name.toLowerCase().endsWith(`.${expectedExtension}`)) {
            validationError = `请选择 .${expectedExtension} 可执行文件`
          } else if (upload.checksum_file.name.toLowerCase() !== `${upload.file.name.toLowerCase()}.sha256`) {
            validationError = 'SHA-256 校验文件名必须与可执行文件匹配'
          }
        }

        if (validationError) {
          result.failures.push({ row_id: upload.row_id, message: validationError })
          continue
        }

        const body = new FormData()
        body.append('os', normalizedTarget.os)
        body.append('package_type', normalizedTarget.package_type)
        body.append('native_arch', normalizedTarget.native_arch)
        body.append('file', upload.file)
        body.append('checksum_file', upload.checksum_file)
        try {
          await api(`/api/admin/agent-releases/${releaseId}/artifacts`, {
            method: 'POST',
            body,
          })
          result.succeeded_row_ids.push(upload.row_id)
        } catch (error) {
          result.failures.push({
            row_id: upload.row_id,
            message: error instanceof Error ? error.message : '上传失败',
          })
        }
      }
      await loadAgentUpdates()
    })

    onComplete(result)
    if (completed && result.succeeded_row_ids.length) {
      agentUpdateMessage.value = `已上传 ${result.succeeded_row_ids.length} 个更新包`
    }
    if (completed && result.failures.length) {
      errorMessage.value = `${result.failures.length} 个更新包上传失败：${result.failures[0].message}`
    }
    agentUpdateOperation.value = null
    agentUpdateBusyId.value = ''
    return completed && result.failures.length === 0
  }

  function deleteAgentArtifact(releaseId: string, artifactId: string) {
    return runAgentUpdateTask('deleting', artifactId, async () => {
      await api(`/api/admin/agent-releases/${releaseId}/artifacts/${artifactId}`, {
        method: 'DELETE',
      })
      await loadAgentUpdates()
    }, '可执行文件已移除')
  }

  function publishAgentRelease(release: AgentRelease, instanceIds: string[] = []) {
    const additionalBatch = release.status === 'published'
    return runAgentUpdateTask('publishing', release.id, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${release.id}/publish`, {
        method: 'POST',
        body: JSON.stringify({ instance_ids: instanceIds }),
      })
      await loadAgentUpdates()
    }, additionalBatch ? `${release.version} 的新增更新包已发布` : `${release.version} 已发布`)
  }

  function addAgentRolloutTargets(release: AgentRelease, instanceIds: string[]) {
    return runAgentUpdateTask('targeting', release.id, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${release.id}/rollout/targets`, {
        method: 'POST',
        body: JSON.stringify({ instance_ids: instanceIds }),
      })
      await loadAgentUpdates()
      await loadAgentRolloutCandidates(release.id)
    }, `已为 ${release.version} 添加 ${instanceIds.length} 个灰度实例`)
  }

  function pauseAgentRollout(release: AgentRelease) {
    return runAgentUpdateTask('pausing', release.id, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${release.id}/rollout/pause`, {
        method: 'POST',
      })
      await loadAgentUpdates()
    }, `${release.version} 已暂停下发新任务`)
  }

  function resumeAgentRollout(release: AgentRelease) {
    return runAgentUpdateTask('resuming', release.id, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${release.id}/rollout/resume`, {
        method: 'POST',
      })
      await loadAgentUpdates()
    }, `${release.version} 已恢复发布`)
  }

  function promoteAgentRollout(release: AgentRelease) {
    return runAgentUpdateTask('promoting', release.id, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${release.id}/rollout/promote`, {
        method: 'POST',
      })
      await loadAgentUpdates()
    }, `${release.version} 已晋级全量`)
  }

  function rollbackAgentRelease(release: AgentRelease) {
    return runAgentUpdateTask('rolling_back', release.id, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${release.id}/rollback`, {
        method: 'POST',
      })
      await loadAgentUpdates()
    }, `${release.version} 已开始批量回滚`)
  }

  function rollbackAgentInstance(release: AgentRelease, attempt: AgentUpdateAttempt) {
    return runAgentUpdateTask('rolling_back', attempt.id, async () => {
      await api<AgentRelease>(
        `/api/admin/agent-releases/${release.id}/instances/${attempt.instance_id}/rollback`,
        { method: 'POST' },
      )
      await loadAgentUpdates()
    }, '已安排实例回滚')
  }

  function reupgradeAgentInstance(release: AgentRelease, attempt: AgentUpdateAttempt) {
    return runAgentUpdateTask('reupgrading', attempt.id, async () => {
      await api<AgentRelease>(
        `/api/admin/agent-releases/${release.id}/instances/${attempt.instance_id}/reupgrade`,
        { method: 'POST' },
      )
      await loadAgentUpdates()
    }, '已安排实例重新升级')
  }

  function deleteAgentRelease(release: AgentRelease) {
    return runAgentUpdateTask('deleting', release.id, async () => {
      await api(`/api/admin/agent-releases/${release.id}`, { method: 'DELETE' })
      await loadAgentUpdates()
    }, release.status === 'published' ? `${release.version} 已永久删除` : '更新草稿已删除')
  }

  function retryAgentUpdateAttempt(attempt: AgentUpdateAttempt) {
    return runAgentUpdateTask('retrying', attempt.id, async () => {
      await api<AgentUpdateAttempt>(`/api/admin/agent-update-attempts/${attempt.id}/retry`, {
        method: 'POST',
      })
      await loadAgentUpdates()
    }, '已安排重新尝试更新')
  }

  async function selectBackgroundImage(event: Event) {
    const input = event.target as HTMLInputElement
    const file = input.files?.[0]
    if (!file) return

    backgroundMessage.value = ''
    errorMessage.value = ''
    if (!['image/png', 'image/jpeg', 'image/webp'].includes(file.type)) {
      errorMessage.value = '仅支持 PNG、JPEG、WebP 图片'
      input.value = ''
      return
    }
    if (file.size > 5 * 1024 * 1024) {
      errorMessage.value = '图片不能超过 5 MB'
      input.value = ''
      return
    }

    backgroundFileName.value = file.name
    backgroundOperation.value = 'uploading'
    const success = await guarded(async () => {
      const body = new FormData()
      body.append('image', file)
      const settings = await api<SettingsResponse>('/api/admin/settings/background-image', {
        method: 'POST',
        body,
      })
      appearance.applyAppearance(settings)
      await loadAdminData()
    })
    input.value = ''
    backgroundFileName.value = ''
    backgroundOperation.value = null
    if (success) backgroundMessage.value = '背景图片已更新并立即生效'
  }

  async function clearBackgroundImage() {
    backgroundMessage.value = ''
    backgroundOperation.value = 'removing'
    const success = await guarded(async () => {
      const settings = await api<SettingsResponse>('/api/admin/settings/background-image', {
        method: 'DELETE',
      })
      appearance.applyAppearance(settings)
      await loadAdminData()
    })
    backgroundOperation.value = null
    backgroundFileName.value = ''
    if (success) backgroundMessage.value = '已恢复默认背景'
  }

  function openTerminal(instance: Instance) {
    terminalState.instance = instance
  }

  function closeTerminal() {
    terminalState.instance = null
  }

  function openRemoteDesktop(instance: Instance) {
    remoteDesktopState.instance = instance
  }

  function closeRemoteDesktop() {
    remoteDesktopState.instance = null
  }

  async function runAgentUpdateTask(
    operation: Exclude<AgentUpdateOperation, null>,
    targetId: string,
    task: () => Promise<void>,
    successMessage: string,
  ) {
    agentUpdateMessage.value = ''
    agentUpdateOperation.value = operation
    agentUpdateBusyId.value = targetId
    const success = await guarded(task)
    if (success) agentUpdateMessage.value = successMessage
    agentUpdateOperation.value = null
    agentUpdateBusyId.value = ''
    return success
  }

  function updateNetworkRates(nextInstances: Instance[]) {
    const counters = new Map<string, { rx: number; tx: number }>()
    for (const instance of nextInstances) {
      if (!instance.online || !instance.metrics) continue
      counters.set(instance.id, {
        rx: instance.metrics.network_rx,
        tx: instance.metrics.network_tx,
      })
    }
    const nextSnapshot: TrafficSnapshot = {
      counters,
      capturedAt: Date.now(),
    }
    const previous = trafficSnapshot.value
    trafficSnapshot.value = nextSnapshot
    networkRxRate.value = 0
    networkTxRate.value = 0
    if (!previous) return

    const elapsedSeconds = (nextSnapshot.capturedAt - previous.capturedAt) / 1000
    if (elapsedSeconds <= 0) return

    let receivedBytes = 0
    let transmittedBytes = 0
    for (const [instanceId, counters] of nextSnapshot.counters) {
      const previousCounters = previous.counters.get(instanceId)
      if (!previousCounters) continue
      receivedBytes += Math.max(0, counters.rx - previousCounters.rx)
      transmittedBytes += Math.max(0, counters.tx - previousCounters.tx)
    }
    networkRxRate.value = receivedBytes / elapsedSeconds
    networkTxRate.value = transmittedBytes / elapsedSeconds
  }

  return {
    instances,
    pendingInstances,
    commands,
    jobs,
    commandExecution,
    audit,
    auditQuery,
    auditLoading,
    auditError,
    auditExporting,
    agentReleases,
    agentUpdateAttempts,
    agentRolloutCandidates,
    agentRolloutCandidatesLoading,
    adminUsers,
    authEnrollments,
    activeAuthEnrollment,
    authMode,
    currentUser,
    isAdmin,
    sessionReady,
    publicReady,
    loading,
    refreshing,
    errorMessage,
    viewMode,
    adminTab,
    backgroundImageUrl: appearance.backgroundImageUrl,
    themeMode: appearance.themeMode,
    resolvedTheme: appearance.resolvedTheme,
    accentColor: appearance.accentColor,
    backgroundFileName,
    backgroundOperation,
    backgroundMessage,
    appearanceMessage,
    agentUpdateOperation,
    agentUpdateBusyId,
    agentUpdateMessage,
    adminResetKey,
    currentTime,
    loginForm,
    userAuthForm,
    settingsForm,
    commandForm,
    agentReleaseForm,
    editInstance,
    editForm,
    terminalState,
    remoteDesktopState,
    onlineCount,
    avgCpu,
    avgMemory,
    totalTraffic,
    totalNetworkRx,
    totalNetworkTx,
    networkRxRate,
    networkTxRate,
    appearanceStyle: appearance.appearanceStyle,
    adminApi: api,
    refreshAll,
    login,
    restartBootstrap,
    logout,
    approveInstance,
    rejectInstance,
    openEdit,
    closeEdit,
    saveEdit,
    disableInstance,
    deleteInstance,
    createCommand,
    removeCommand,
    runCommand,
    closeCommandExecution,
    saveSettings,
    loadAudit,
    updateAuditQuery,
    setAuditPage,
    exportAudit,
    saveAppearance,
    createUserEnrollment,
    createDeviceEnrollment,
    confirmAuthEnrollment,
    cancelAuthEnrollment,
    setAdminUserEnabled,
    deleteAdminUser,
    revokeAuthenticatorDevice,
    loadAgentUpdates,
    loadAgentRolloutCandidates,
    createAgentRelease,
    saveAgentRelease,
    uploadAgentArtifact,
    deleteAgentArtifact,
    publishAgentRelease,
    addAgentRolloutTargets,
    pauseAgentRollout,
    resumeAgentRollout,
    promoteAgentRollout,
    rollbackAgentRelease,
    rollbackAgentInstance,
    reupgradeAgentInstance,
    deleteAgentRelease,
    retryAgentUpdateAttempt,
    selectBackgroundImage,
    clearBackgroundImage,
    openTerminal,
    closeTerminal,
    openRemoteDesktop,
    closeRemoteDesktop,
  }
}
