import { computed, onMounted, reactive, ref } from 'vue'
import { api } from '../api/http'
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
  const username = ref<string | null>(null)
  const loading = ref(false)
  const errorMessage = ref('')
  const viewMode = ref<ViewMode>('grid')
  const adminTab = ref<AdminTab>('pending')
  const backgroundImageUrl = ref<string | null>(null)
  const currentTime = ref(new Date())
  const backgroundFileName = ref('')
  const trafficSnapshot = ref<TrafficSnapshot | null>(null)
  const networkRxRate = ref(0)
  const networkTxRate = ref(0)

  const loginForm = reactive({
    username: 'admin',
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
    region: '',
    remark: '',
  })

  const terminalState = reactive<{
    instance: Instance | null
    socket: WebSocket | null
    output: string
    input: string
    connected: boolean
  }>({
    instance: null,
    socket: null,
    output: '',
    input: '',
    connected: false,
  })

  const onlineCount = computed(() => instances.value.filter((item) => item.online).length)
  const avgCpu = computed(() => average(instances.value.map((item) => item.metrics?.cpu_percent)))
  const avgMemory = computed(() =>
    average(
      instances.value.map((item) =>
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
    await Promise.all([loadAppearance(), loadPublic(), checkSession()])
    window.setInterval(loadPublic, 5000)
    window.setInterval(() => {
      currentTime.value = new Date()
    }, 1000)
  })

  async function guarded(task: () => Promise<void>) {
    loading.value = true
    errorMessage.value = ''
    try {
      await task()
    } catch (error) {
      errorMessage.value = error instanceof Error ? error.message : '操作失败'
    } finally {
      loading.value = false
    }
  }

  async function loadPublic() {
    const nextInstances = await api<Instance[]>('/api/public/instances')
    updateNetworkRates(nextInstances)
    instances.value = nextInstances
  }

  async function loadAppearance() {
    const appearance = await api<AppearanceResponse>('/api/public/appearance')
    applyAppearance(appearance.background_image_url)
  }

  async function checkSession() {
    const me = await api<{ authenticated: boolean; username: string | null }>('/api/admin/me')
    isAdmin.value = me.authenticated
    username.value = me.username
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
    guarded(async () => {
      await Promise.all([loadPublic(), loadAppearance()])
      if (isAdmin.value) {
        await loadAdminData()
      }
    })
  }

  function login() {
    guarded(async () => {
      const me = await api<{ username: string }>('/api/admin/login', {
        method: 'POST',
        body: JSON.stringify(loginForm),
      })
      isAdmin.value = true
      username.value = me.username
      loginForm.password = ''
      await loadAdminData()
    })
  }

  function logout() {
    guarded(async () => {
      await api('/api/admin/logout', { method: 'POST' })
      isAdmin.value = false
      username.value = null
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
    editInstance.value = instance
    editForm.name = instance.name
    editForm.region = instance.region
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
        body: JSON.stringify(editForm),
      })
      editInstance.value = null
      await loadPublic()
    })
  }

  function disableInstance(instance: Instance) {
    if (!window.confirm(`停用实例 ${instance.name}？停用后将不再接受上报。`)) return
    guarded(async () => {
      await api(`/api/admin/instances/${instance.id}/disable`, { method: 'POST' })
      await loadPublic()
    })
  }

  function deleteInstance(instance: Instance) {
    if (!window.confirm(`删除实例 ${instance.name} 及历史指标？此操作不可恢复。`)) return
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
    if (!window.confirm(`停用快捷操作 ${command.name}？`)) return
    guarded(async () => {
      await api(`/api/admin/commands/${command.id}`, { method: 'DELETE' })
      await loadAdminData()
    })
  }

  function runCommand(instance: Instance, command: CommandRecord) {
    const prompt = command.confirm_text || `确认在 ${instance.name} 上执行：${command.command}`
    if (!window.confirm(prompt)) return
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

  function selectBackgroundImage(event: Event) {
    const input = event.target as HTMLInputElement
    const file = input.files?.[0]
    if (!file) return
    backgroundFileName.value = file.name
    guarded(async () => {
      const body = new FormData()
      body.append('image', file)
      const settings = await api<SettingsResponse>('/api/admin/settings/background-image', {
        method: 'POST',
        body,
      })
      applyAppearance(settings.background_image_url)
      settingsForm.background_image_url = settings.background_image_url
      input.value = ''
      backgroundFileName.value = ''
      await loadAdminData()
    })
  }

  function clearBackgroundImage() {
    guarded(async () => {
      const settings = await api<SettingsResponse>('/api/admin/settings/background-image', {
        method: 'DELETE',
      })
      applyAppearance(settings.background_image_url)
      settingsForm.background_image_url = settings.background_image_url
      backgroundFileName.value = ''
      await loadAdminData()
    })
  }

  function openTerminal(instance: Instance) {
    closeTerminal()
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const socket = new WebSocket(
      `${protocol}//${window.location.host}/api/admin/instances/${instance.id}/terminal/ws`,
    )
    terminalState.instance = instance
    terminalState.socket = socket
    terminalState.output = ''
    terminalState.input = ''
    terminalState.connected = false

    socket.onopen = () => {
      terminalState.connected = true
    }
    socket.onmessage = (event) => {
      terminalState.output += String(event.data)
    }
    socket.onerror = () => {
      terminalState.output += '\n连接发生错误。\n'
    }
    socket.onclose = () => {
      terminalState.connected = false
    }
  }

  function closeTerminal() {
    terminalState.socket?.close()
    terminalState.instance = null
    terminalState.socket = null
    terminalState.output = ''
    terminalState.input = ''
    terminalState.connected = false
  }

  function sendTerminalCommand() {
    const command = terminalState.input.trim()
    if (!command || !terminalState.socket || terminalState.socket.readyState !== WebSocket.OPEN) {
      return
    }
    terminalState.output += `> ${command}\n`
    terminalState.socket.send(command)
    terminalState.input = ''
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
    username,
    loading,
    errorMessage,
    viewMode,
    adminTab,
    backgroundImageUrl,
    backgroundFileName,
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
    sendTerminalCommand,
  }
}
