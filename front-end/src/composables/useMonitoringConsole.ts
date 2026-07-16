import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { api } from '../api/http'
import { getCountryOption } from '../data/countries'
import { useAppearance } from './useAppearance'
import type {
  ActionLog,
  AgentArtifactTarget,
  AgentArtifactUploadItem,
  AgentArtifactUploadResult,
  AgentRelease,
  AgentReleaseForm,
  AgentUpdateAttempt,
  AdminUser,
  AdminUsersResponse,
  AdminTab,
  AppearanceResponse,
  AuthEnrollment,
  AuthMode,
  CommandJob,
  CommandRecord,
  Instance,
  PendingInstance,
  PendingAuthEnrollment,
  SessionUser,
  SettingsResponse,
  ViewMode,
} from '../types/domain'
import { average } from '../utils/format'

type TrafficSnapshot = {
  rx: number
  tx: number
  capturedAt: number
}

type AgentUpdateOperation = 'creating' | 'saving' | 'uploading' | 'publishing' | 'deleting' | 'retrying' | null

export function useMonitoringConsole() {
  const appearance = useAppearance()
  const instances = ref<Instance[]>([])
  const pendingInstances = ref<PendingInstance[]>([])
  const commands = ref<CommandRecord[]>([])
  const jobs = ref<CommandJob[]>([])
  const logs = ref<ActionLog[]>([])
  const agentReleases = ref<AgentRelease[]>([])
  const agentUpdateAttempts = ref<AgentUpdateAttempt[]>([])
  const adminUsers = ref<AdminUser[]>([])
  const authEnrollments = ref<PendingAuthEnrollment[]>([])
  const activeAuthEnrollment = ref<AuthEnrollment | null>(null)
  const authMode = ref<AuthMode>('totp')
  const currentUser = ref<SessionUser | null>(null)
  const isAdmin = ref(false)
  const sessionReady = ref(false)
  const publicReady = ref(false)
  const loading = ref(false)
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
  const trafficSnapshot = ref<TrafficSnapshot | null>(null)
  const networkRxRate = ref(0)
  const networkTxRate = ref(0)
  let publicPollTimer: number | null = null
  let clockTimer: number | null = null
  let publicRequestInFlight = false

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
  })

  async function guarded(task: () => Promise<void>): Promise<boolean> {
    loading.value = true
    errorMessage.value = ''
    try {
      await task()
      return true
    } catch (error) {
      errorMessage.value = error instanceof Error ? error.message : '操作失败'
      return false
    } finally {
      loading.value = false
    }
  }

  async function loadPublic() {
    const nextInstances = await api<Instance[]>('/api/public/instances')
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
    const response = await api<AppearanceResponse>('/api/public/appearance')
    appearance.applyAppearance(response)
  }

  async function checkSession() {
    const [status, me] = await Promise.all([
      api<{ mode: AuthMode }>('/api/admin/auth/status'),
      api<{ authenticated: boolean; user: SessionUser | null }>('/api/admin/me'),
    ])
    authMode.value = status.mode
    isAdmin.value = me.authenticated
    currentUser.value = me.user
    if (me.authenticated) {
      await loadAdminData()
    }
  }

  async function loadAdminData() {
    const [pending, commandList, jobList, logList, settings, users] = await Promise.all([
      api<PendingInstance[]>('/api/admin/pending-instances'),
      api<CommandRecord[]>('/api/admin/commands'),
      api<CommandJob[]>('/api/admin/jobs'),
      api<ActionLog[]>('/api/admin/logs'),
      api<SettingsResponse>('/api/admin/settings'),
      api<AdminUsersResponse>('/api/admin/users'),
      loadAgentUpdates(),
    ])
    pendingInstances.value = pending
    commands.value = commandList
    jobs.value = jobList
    logs.value = logList
    settingsForm.retention_days = settings.retention_days
    appearance.applyAppearance(settings)
    settingsForm.background_image_url = settings.background_image_url
    settingsForm.theme_mode = settings.theme_mode
    settingsForm.accent_color = settings.accent_color
    applyUsers(users)
  }

  async function loadAgentUpdates() {
    const [releases, attempts] = await Promise.all([
      api<AgentRelease[]>('/api/admin/agent-releases'),
      api<AgentUpdateAttempt[]>('/api/admin/agent-update-attempts'),
    ])
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
      },
    }))
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
      isAdmin.value = true
      currentUser.value = response.user
      authMode.value = 'totp'
      activeAuthEnrollment.value = null
      loginForm.password = ''
      loginForm.code = ''
      await loadAdminData()
    })
  }

  function restartBootstrap() {
    activeAuthEnrollment.value = null
    loginForm.code = ''
    errorMessage.value = ''
  }

  function logout() {
    guarded(async () => {
      await api('/api/admin/logout', { method: 'POST' })
      terminalState.instance = null
      remoteDesktopState.instance = null
      isAdmin.value = false
      currentUser.value = null
      pendingInstances.value = []
      commands.value = []
      jobs.value = []
      logs.value = []
      agentReleases.value = []
      agentUpdateAttempts.value = []
      adminUsers.value = []
      authEnrollments.value = []
      activeAuthEnrollment.value = null
      agentUpdateMessage.value = ''
    })
  }

  function approveInstance(id: string) {
    guarded(async () => {
      await api(`/api/admin/pending-instances/${id}/approve`, { method: 'POST' })
      await Promise.all([loadPublic(), loadAdminData()])
    })
  }

  function rejectInstance(id: string) {
    guarded(async () => {
      await api(`/api/admin/pending-instances/${id}/reject`, { method: 'POST' })
      await loadAdminData()
    })
  }

  function openEdit(instance: Instance) {
    const country = getCountryOption(instance.country_code)
    editInstance.value = instance
    editForm.name = instance.name
    editForm.country_code = country?.code || ''
    editForm.country = country?.name || ''
    editForm.remark = instance.remark
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
          ...editForm,
          province_code: '',
          province: '',
          city: '',
        }),
      })
      editInstance.value = null
      await loadPublic()
    })
  }

  function disableInstance(instance: Instance) {
    guarded(async () => {
      await api(`/api/admin/instances/${instance.id}/disable`, { method: 'POST' })
      await loadPublic()
    })
  }

  function deleteInstance(instance: Instance) {
    guarded(async () => {
      await api(`/api/admin/instances/${instance.id}`, { method: 'DELETE' })
      await Promise.all([loadPublic(), loadAdminData()])
    })
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
    })
  }

  function removeCommand(command: CommandRecord) {
    guarded(async () => {
      await api(`/api/admin/commands/${command.id}`, { method: 'DELETE' })
      await loadAdminData()
    })
  }

  function runCommand(instance: Instance, command: CommandRecord) {
    guarded(async () => {
      await api(`/api/admin/instances/${instance.id}/commands/${command.id}/run`, {
        method: 'POST',
      })
      await loadAdminData()
    })
  }

  function saveSettings() {
    guarded(async () => {
      await api('/api/admin/settings', {
        method: 'PUT',
        body: JSON.stringify({ retention_days: settingsForm.retention_days }),
      })
      await loadAdminData()
    })
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
    })
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
    applyUsers(await api<AdminUsersResponse>('/api/admin/users'))
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

  function publishAgentRelease(release: AgentRelease) {
    return runAgentUpdateTask('publishing', release.id, async () => {
      await api<AgentRelease>(`/api/admin/agent-releases/${release.id}/publish`, {
        method: 'POST',
      })
      await loadAgentUpdates()
    }, `${release.version} 已发布`)
  }

  function deleteAgentRelease(release: AgentRelease) {
    return runAgentUpdateTask('deleting', release.id, async () => {
      await api(`/api/admin/agent-releases/${release.id}`, { method: 'DELETE' })
      await loadAgentUpdates()
    }, '更新草稿已删除')
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
    const nextSnapshot = {
      rx: nextInstances.reduce((sum, item) => sum + (item.metrics?.network_rx || 0), 0),
      tx: nextInstances.reduce((sum, item) => sum + (item.metrics?.network_tx || 0), 0),
      capturedAt: Date.now(),
    }
    const previous = trafficSnapshot.value
    trafficSnapshot.value = nextSnapshot
    if (!previous) return

    const elapsedSeconds = (nextSnapshot.capturedAt - previous.capturedAt) / 1000
    if (elapsedSeconds <= 0) return
    networkRxRate.value = Math.max(0, (nextSnapshot.rx - previous.rx) / elapsedSeconds)
    networkTxRate.value = Math.max(0, (nextSnapshot.tx - previous.tx) / elapsedSeconds)
  }

  return {
    instances,
    pendingInstances,
    commands,
    jobs,
    logs,
    agentReleases,
    agentUpdateAttempts,
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
    saveSettings,
    saveAppearance,
    createUserEnrollment,
    createDeviceEnrollment,
    confirmAuthEnrollment,
    cancelAuthEnrollment,
    setAdminUserEnabled,
    deleteAdminUser,
    revokeAuthenticatorDevice,
    loadAgentUpdates,
    createAgentRelease,
    saveAgentRelease,
    uploadAgentArtifact,
    deleteAgentArtifact,
    publishAgentRelease,
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
