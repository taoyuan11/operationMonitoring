import { computed, onBeforeUnmount, onMounted, reactive, ref } from 'vue'
import { api } from '../api/http'
import { getCountryOption } from '../data/countries'
import type {
  ActionLog,
  AdminTab,
  AppearanceResponse,
  CommandJob,
  CommandRecord,
  Instance,
  PendingInstance,
  SettingsResponse,
  ViewMode,
} from '../types/domain'
import { average } from '../utils/format'

type TrafficSnapshot = {
  rx: number
  tx: number
  capturedAt: number
}

export function useMonitoringConsole() {
  const instances = ref<Instance[]>([])
  const pendingInstances = ref<PendingInstance[]>([])
  const commands = ref<CommandRecord[]>([])
  const jobs = ref<CommandJob[]>([])
  const logs = ref<ActionLog[]>([])
  const isAdmin = ref(false)
  const sessionReady = ref(false)
  const publicReady = ref(false)
  const loading = ref(false)
  const refreshing = ref(false)
  const errorMessage = ref('')
  const viewMode = ref<ViewMode>('grid')
  const adminTab = ref<AdminTab>('pending')
  const backgroundImageUrl = ref<string | null>(null)
  const currentTime = ref(new Date())
  const backgroundFileName = ref('')
  const backgroundOperation = ref<'uploading' | 'removing' | null>(null)
  const backgroundMessage = ref('')
  const trafficSnapshot = ref<TrafficSnapshot | null>(null)
  const networkRxRate = ref(0)
  const networkTxRate = ref(0)
  let publicPollTimer: number | null = null
  let clockTimer: number | null = null
  let publicRequestInFlight = false

  const loginForm = reactive({
    password: '',
  })

  const settingsForm = reactive({
    retention_days: 30,
    background_image_url: null as string | null,
  })

  const commandForm = reactive({
    name: '',
    command: '',
    confirm_text: '',
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
  const appearanceStyle = computed<Record<string, string>>(() => {
    if (!backgroundImageUrl.value) return {} as Record<string, string>
    return {
      '--site-background-image': `url("${backgroundImageUrl.value}")`,
    }
  })

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
      await loadPublic()
    } catch {
      // Keep the last valid snapshot during transient network failures.
    } finally {
      publicRequestInFlight = false
    }
  }

  async function loadAppearance() {
    const appearance = await api<AppearanceResponse>('/api/public/appearance')
    applyAppearance(appearance.background_image_url)
  }

  async function checkSession() {
    const me = await api<{ authenticated: boolean }>('/api/admin/me')
    isAdmin.value = me.authenticated
    if (me.authenticated) {
      await loadAdminData()
    }
  }

  async function loadAdminData() {
    const [pending, commandList, jobList, logList, settings] = await Promise.all([
      api<PendingInstance[]>('/api/admin/pending-instances'),
      api<CommandRecord[]>('/api/admin/commands'),
      api<CommandJob[]>('/api/admin/jobs'),
      api<ActionLog[]>('/api/admin/logs'),
      api<SettingsResponse>('/api/admin/settings'),
    ])
    pendingInstances.value = pending
    commands.value = commandList
    jobs.value = jobList
    logs.value = logList
    settingsForm.retention_days = settings.retention_days
    applyAppearance(settings.background_image_url)
    settingsForm.background_image_url = settings.background_image_url
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
      await api<{ role: 'admin' }>('/api/admin/login', {
        method: 'POST',
        body: JSON.stringify(loginForm),
      })
      isAdmin.value = true
      loginForm.password = ''
      await loadAdminData()
    })
  }

  function logout() {
    guarded(async () => {
      await api('/api/admin/logout', { method: 'POST' })
      isAdmin.value = false
      pendingInstances.value = []
      commands.value = []
      jobs.value = []
      logs.value = []
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
      applyAppearance(settings.background_image_url)
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
      applyAppearance(settings.background_image_url)
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

  function applyAppearance(url: string | null) {
    backgroundImageUrl.value = url
    settingsForm.background_image_url = url
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
    isAdmin,
    sessionReady,
    publicReady,
    loading,
    refreshing,
    errorMessage,
    viewMode,
    adminTab,
    backgroundImageUrl,
    backgroundFileName,
    backgroundOperation,
    backgroundMessage,
    currentTime,
    loginForm,
    settingsForm,
    commandForm,
    editInstance,
    editForm,
    terminalState,
    onlineCount,
    avgCpu,
    avgMemory,
    totalTraffic,
    totalNetworkRx,
    totalNetworkTx,
    networkRxRate,
    networkTxRate,
    appearanceStyle,
    refreshAll,
    login,
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
    selectBackgroundImage,
    clearBackgroundImage,
    openTerminal,
    closeTerminal,
  }
}
