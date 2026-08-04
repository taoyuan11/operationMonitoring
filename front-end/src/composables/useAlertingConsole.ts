import { computed, onBeforeUnmount, onMounted, reactive, ref, watch, type Ref } from 'vue'
import {
  acknowledgeAlertEvent,
  createAlertChannel,
  createAlertMaintenance,
  createAlertRule,
  deleteAlertChannel,
  deleteAlertMaintenance,
  deleteAlertRule,
  getAlertDelivery,
  getAlertEvent,
  getAlertSummary,
  listAlertChannels,
  listAlertDeliveries,
  listAlertEvents,
  listAlertMaintenance,
  listAlertRules,
  retryAlertDelivery,
  setAlertRuleEnabled,
  testAlertChannel,
  updateAlertChannel,
  updateAlertMaintenance,
  updateAlertRule,
  type AlertApiRequest,
} from '../api/alerts'
import type {
  AlertCenterTab,
  AlertDelivery,
  AlertDeliveryDetail,
  AlertDeliveryQuery,
  AlertEvent,
  AlertEventDetail,
  AlertEventQuery,
  AlertMaintenanceInput,
  AlertMaintenanceWindow,
  AlertMetric,
  AlertPage,
  AlertRule,
  AlertRuleInput,
  AlertSummary,
  AlertWebhookChannel,
  AlertWebhookChannelInput,
} from '../types/domain'

type AlertingOptions = {
  isAdmin: Ref<boolean>
  active: Ref<boolean>
  request: AlertApiRequest
}

export type AlertHeaderDraft = {
  id: number
  name: string
  value: string
}

const EMPTY_SUMMARY: AlertSummary = {
  firing: 0,
  acknowledged: 0,
  suppressed: 0,
  resolved_24h: 0,
}

const EMPTY_PAGE = <T>(pageSize = 50): AlertPage<T> => ({
  items: [],
  page: 1,
  page_size: pageSize,
  total: 0,
  pages: 0,
})

export const ALERT_RULE_PRESETS: Array<{
  metric: AlertMetric
  name: string
  threshold: number | null
  duration_seconds: number
}> = [
  { metric: 'node_offline', name: '节点离线', threshold: null, duration_seconds: 60 },
  { metric: 'cpu_percent', name: 'CPU 使用率过高', threshold: 90, duration_seconds: 300 },
  { metric: 'memory_percent', name: '内存使用率过高', threshold: 90, duration_seconds: 300 },
  { metric: 'disk_percent', name: '磁盘使用率过高', threshold: 90, duration_seconds: 300 },
  { metric: 'latency_ms', name: '节点延迟过高', threshold: 500, duration_seconds: 120 },
]

function defaultEventQuery(): AlertEventQuery {
  return {
    page: 1,
    page_size: 50,
    status: '',
    severity: '',
    metric: '',
    instance_id: '',
    suppressed: '',
    from: null,
    to: null,
    search: '',
  }
}

function defaultDeliveryQuery(): AlertDeliveryQuery {
  return {
    page: 1,
    page_size: 50,
    status: '',
    kind: '',
    channel_id: '',
    event_id: '',
  }
}

function defaultRuleInput(): AlertRuleInput {
  const preset = ALERT_RULE_PRESETS[0]
  return {
    name: preset.name,
    metric: preset.metric,
    threshold: preset.threshold,
    duration_seconds: preset.duration_seconds,
    severity: 'critical',
    scope: 'all',
    target_instance_ids: [],
    channel_ids: [],
    enabled: true,
  }
}

function defaultMaintenanceInput(): AlertMaintenanceInput {
  const startsAt = Math.floor(Date.now() / 1000) + 5 * 60
  return {
    name: '',
    reason: '',
    scope: 'global',
    target_ids: [],
    starts_at: startsAt,
    ends_at: startsAt + 60 * 60,
    enabled: true,
  }
}

export function useAlertingConsole(options: AlertingOptions) {
  const activeTab = ref<AlertCenterTab>('events')
  const summary = ref<AlertSummary>({ ...EMPTY_SUMMARY })
  const events = ref<AlertPage<AlertEvent>>(EMPTY_PAGE())
  const rules = ref<AlertPage<AlertRule>>(EMPTY_PAGE(200))
  const maintenance = ref<AlertPage<AlertMaintenanceWindow>>(EMPTY_PAGE(200))
  const channels = ref<AlertPage<AlertWebhookChannel>>(EMPTY_PAGE(200))
  const deliveries = ref<AlertPage<AlertDelivery>>(EMPTY_PAGE())
  const eventDetail = ref<AlertEventDetail | null>(null)
  const deliveryDetail = ref<AlertDeliveryDetail | null>(null)
  const expandedEventId = ref('')
  const expandedDeliveryId = ref('')
  const acknowledgeNote = ref('')
  const eventQuery = reactive<AlertEventQuery>(defaultEventQuery())
  const deliveryQuery = reactive<AlertDeliveryQuery>(defaultDeliveryQuery())
  const ruleForm = reactive<AlertRuleInput>(defaultRuleInput())
  const maintenanceForm = reactive<AlertMaintenanceInput>(defaultMaintenanceInput())
  const channelForm = reactive({
    name: '',
    url: '',
    secret: '',
    clear_secret: false,
    replace_headers: false,
    enabled: true,
    headers: [] as AlertHeaderDraft[],
  })
  const editingRuleId = ref('')
  const editingMaintenanceId = ref('')
  const editingChannelId = ref('')
  const loadingKeys = ref<string[]>([])
  const errorMessage = ref('')
  const successMessage = ref('')
  const loadedTabs = new Set<AlertCenterTab>()
  let summaryRequest = 0
  let eventRequest = 0
  let detailRequest = 0
  let deliveryRequest = 0
  let deliveryDetailRequest = 0
  let headerId = 0
  let pollTimer: number | null = null

  const attentionCount = computed(() => summary.value.firing)
  const loading = computed(() => loadingKeys.value.length > 0)

  onMounted(() => {
    pollTimer = window.setInterval(() => {
      if (!options.isAdmin.value) return
      void loadSummary(true)
      if (options.active.value && activeTab.value === 'events') void loadEvents(true)
    }, 5000)
  })

  onBeforeUnmount(() => {
    if (pollTimer !== null) window.clearInterval(pollTimer)
  })

  watch(
    () => options.isAdmin.value,
    (isAdmin) => {
      if (isAdmin) {
        void loadSummary(true)
        if (options.active.value) void loadTab(activeTab.value)
      } else {
        clearState()
      }
    },
    { immediate: true },
  )

  watch(
    [() => options.active.value, activeTab],
    ([isActive, tab]) => {
      if (isActive && options.isAdmin.value) void loadTab(tab)
    },
  )

  function isBusy(key: string) {
    return loadingKeys.value.includes(key)
  }

  async function run<T>(key: string, task: () => Promise<T>, message = ''): Promise<T | null> {
    if (isBusy(key)) return null
    loadingKeys.value = [...loadingKeys.value, key]
    errorMessage.value = ''
    successMessage.value = ''
    try {
      const result = await task()
      if (message) successMessage.value = message
      return result
    } catch (error) {
      if (!(error instanceof Error && error.name === 'AbortError')) {
        errorMessage.value = error instanceof Error ? error.message : '告警操作失败'
      }
      return null
    } finally {
      loadingKeys.value = loadingKeys.value.filter((item) => item !== key)
    }
  }

  function clearState() {
    summaryRequest += 1
    eventRequest += 1
    detailRequest += 1
    deliveryRequest += 1
    deliveryDetailRequest += 1
    summary.value = { ...EMPTY_SUMMARY }
    events.value = EMPTY_PAGE()
    rules.value = EMPTY_PAGE(200)
    maintenance.value = EMPTY_PAGE(200)
    channels.value = EMPTY_PAGE(200)
    deliveries.value = EMPTY_PAGE()
    eventDetail.value = null
    deliveryDetail.value = null
    expandedEventId.value = ''
    expandedDeliveryId.value = ''
    Object.assign(eventQuery, defaultEventQuery())
    Object.assign(deliveryQuery, defaultDeliveryQuery())
    resetRuleForm()
    resetMaintenanceForm()
    resetChannelForm()
    loadedTabs.clear()
    loadingKeys.value = []
    errorMessage.value = ''
    successMessage.value = ''
    activeTab.value = 'events'
  }

  async function loadSummary(silent = false) {
    if (!options.isAdmin.value) return
    const request = ++summaryRequest
    const task = async () => {
      const response = await getAlertSummary(options.request)
      if (request === summaryRequest) summary.value = { ...EMPTY_SUMMARY, ...response }
    }
    if (silent) {
      try {
        await task()
      } catch {
        // Preserve the last badge snapshot during transient failures.
      }
      return
    }
    await run('summary:load', task)
  }

  async function loadEvents(silent = false) {
    if (!options.isAdmin.value) return
    const request = ++eventRequest
    const query = { ...eventQuery }
    const task = async () => {
      let response = await listAlertEvents(query, options.request)
      const lastPage = Math.max(response.pages, 1)
      if (response.page > lastPage) {
        query.page = lastPage
        response = await listAlertEvents(query, options.request)
      }
      if (request !== eventRequest) return
      events.value = response
      eventQuery.page = response.page
      eventQuery.page_size = response.page_size
      loadedTabs.add('events')
      if (expandedEventId.value && response.items.some((item) => item.id === expandedEventId.value)) {
        void loadEventDetail(expandedEventId.value, true)
      }
    }
    if (silent) {
      try {
        await task()
      } catch {
        // Keep the last event list while polling recovers.
      }
      return
    }
    await run('events:load', task)
  }

  async function loadRules() {
    await run('rules:load', async () => {
      rules.value = await loadEveryPage((page) => listAlertRules(page, 200, options.request))
      loadedTabs.add('rules')
    })
  }

  async function loadMaintenance() {
    await run('maintenance:load', async () => {
      maintenance.value = await loadEveryPage((page) =>
        listAlertMaintenance(page, 200, options.request),
      )
      loadedTabs.add('maintenance')
    })
  }

  async function loadChannels() {
    await run('channels:load', async () => {
      channels.value = await loadEveryPage((page) => listAlertChannels(page, 200, options.request))
      loadedTabs.add('webhooks')
    })
  }

  async function loadDeliveries() {
    const request = ++deliveryRequest
    const query = { ...deliveryQuery }
    await run('deliveries:load', async () => {
      let response = await listAlertDeliveries(query, options.request)
      const lastPage = Math.max(response.pages, 1)
      if (response.page > lastPage) {
        query.page = lastPage
        response = await listAlertDeliveries(query, options.request)
      }
      if (request !== deliveryRequest) return
      deliveries.value = response
      deliveryQuery.page = response.page
      deliveryQuery.page_size = response.page_size
      loadedTabs.add('deliveries')
      if (
        expandedDeliveryId.value
        && response.items.some((item) => item.id === expandedDeliveryId.value)
      ) {
        void loadDeliveryDetail(expandedDeliveryId.value, true)
      }
    })
  }

  async function loadTab(tab: AlertCenterTab) {
    if (tab === 'events') await loadEvents()
    if (tab === 'rules') await Promise.all([loadRules(), loadChannels()])
    if (tab === 'maintenance') await Promise.all([loadMaintenance(), loadRules()])
    if (tab === 'webhooks') await loadChannels()
    if (tab === 'deliveries') await Promise.all([loadDeliveries(), loadChannels()])
  }

  async function loadEveryPage<T>(fetchPage: (page: number) => Promise<AlertPage<T>>) {
    const first = await fetchPage(1)
    if (first.pages <= 1) return first
    const remaining: AlertPage<T>[] = []
    for (let page = 2; page <= first.pages; page += 1) {
      remaining.push(await fetchPage(page))
    }
    return {
      ...first,
      items: [first, ...remaining].flatMap((page) => page.items),
    }
  }

  function refreshCurrentTab() {
    loadedTabs.delete(activeTab.value)
    void Promise.all([loadSummary(), loadTab(activeTab.value)])
  }

  function updateEventQuery(patch: Partial<AlertEventQuery>) {
    Object.assign(eventQuery, patch)
    eventQuery.page = 1
    void loadEvents()
  }

  function resetEventQuery() {
    Object.assign(eventQuery, defaultEventQuery())
    void loadEvents()
  }

  function setEventPage(page: number) {
    const next = Math.max(1, Math.min(page, events.value.pages || 1))
    if (next === eventQuery.page) return
    eventQuery.page = next
    void loadEvents()
  }

  async function toggleEventDetail(event: AlertEvent) {
    if (expandedEventId.value === event.id) {
      expandedEventId.value = ''
      eventDetail.value = null
      acknowledgeNote.value = ''
      return
    }
    expandedEventId.value = event.id
    eventDetail.value = null
    acknowledgeNote.value = ''
    await loadEventDetail(event.id)
  }

  async function loadEventDetail(id: string, silent = false) {
    const request = ++detailRequest
    const task = async () => {
      const detail = await getAlertEvent(id, options.request)
      if (request === detailRequest && expandedEventId.value === id) eventDetail.value = detail
    }
    if (silent) {
      try {
        await task()
      } catch {
        // Detail refresh is best effort during polling.
      }
      return
    }
    await run(`event:${id}:load`, task)
  }

  async function acknowledgeEvent(event: AlertEvent) {
    const response = await run(
      `event:${event.id}:acknowledge`,
      () => acknowledgeAlertEvent(event.id, acknowledgeNote.value.trim(), options.request),
      '事件已确认',
    )
    if (!response) return false
    acknowledgeNote.value = ''
    await Promise.all([loadSummary(true), loadEvents(true), loadEventDetail(event.id, true)])
    return true
  }

  function selectRulePreset(metric: AlertMetric) {
    const preset = ALERT_RULE_PRESETS.find((item) => item.metric === metric)
    if (!preset) return
    editingRuleId.value = ''
    Object.assign(ruleForm, {
      ...defaultRuleInput(),
      name: preset.name,
      metric: preset.metric,
      threshold: preset.threshold,
      duration_seconds: preset.duration_seconds,
      severity: metric === 'node_offline' ? 'critical' : 'warning',
    })
  }

  function editRule(rule: AlertRule) {
    editingRuleId.value = rule.id
    Object.assign(ruleForm, {
      name: rule.name,
      metric: rule.metric,
      threshold: rule.threshold,
      duration_seconds: rule.duration_seconds,
      severity: rule.severity,
      scope: rule.scope,
      target_instance_ids: [...rule.target_instance_ids],
      channel_ids: [...rule.channel_ids],
      enabled: rule.enabled,
    })
  }

  function resetRuleForm() {
    editingRuleId.value = ''
    Object.assign(ruleForm, defaultRuleInput())
  }

  async function saveRule() {
    if (!ruleForm.name.trim()) {
      errorMessage.value = '请输入规则名称'
      return false
    }
    if (ruleForm.scope === 'specific' && !ruleForm.target_instance_ids.length) {
      errorMessage.value = '指定节点规则至少选择一个节点'
      return false
    }
    const threshold = Number(ruleForm.threshold)
    if (
      ruleForm.metric !== 'node_offline'
      && (!Number.isFinite(threshold)
        || threshold < 0
        || (ruleForm.metric !== 'latency_ms' && threshold > 100))
    ) {
      errorMessage.value = ruleForm.metric === 'latency_ms'
        ? '延迟阈值必须是非负数值'
        : '百分比阈值必须在 0 到 100 之间'
      return false
    }
    const payload: AlertRuleInput = {
      ...ruleForm,
      name: ruleForm.name.trim(),
      threshold: ruleForm.metric === 'node_offline' ? null : threshold,
      duration_seconds: Math.max(0, Number(ruleForm.duration_seconds)),
      target_instance_ids: ruleForm.scope === 'specific' ? [...ruleForm.target_instance_ids] : [],
      channel_ids: [...ruleForm.channel_ids],
    }
    const id = editingRuleId.value
    const response = await run(
      id ? `rule:${id}:save` : 'rule:create',
      () => id
        ? updateAlertRule(id, payload, options.request)
        : createAlertRule(payload, options.request),
      id ? '规则已更新' : '规则已创建',
    )
    if (!response) return false
    resetRuleForm()
    await Promise.all([loadRules(), loadSummary(true), loadEvents(true)])
    return true
  }

  async function toggleRule(rule: AlertRule, enabled: boolean) {
    const response = await run(
      `rule:${rule.id}:enabled`,
      () => setAlertRuleEnabled(rule.id, enabled, options.request),
      enabled ? '规则已启用' : '规则已停用',
    )
    if (!response) return false
    await Promise.all([loadRules(), loadSummary(true), loadEvents(true)])
    return true
  }

  async function removeRule(rule: AlertRule) {
    const response = await run(
      `rule:${rule.id}:delete`,
      async () => {
        await deleteAlertRule(rule.id, options.request)
        return true
      },
      '规则已删除',
    )
    if (!response) return false
    if (editingRuleId.value === rule.id) resetRuleForm()
    await Promise.all([loadRules(), loadSummary(true), loadEvents(true)])
    return true
  }

  function editMaintenance(window: AlertMaintenanceWindow) {
    editingMaintenanceId.value = window.id
    Object.assign(maintenanceForm, {
      name: window.name,
      reason: window.reason,
      scope: window.scope,
      target_ids: [...window.target_ids],
      starts_at: window.starts_at,
      ends_at: window.ends_at,
      enabled: window.enabled,
    })
  }

  function resetMaintenanceForm() {
    editingMaintenanceId.value = ''
    Object.assign(maintenanceForm, defaultMaintenanceInput())
  }

  async function saveMaintenance() {
    if (!maintenanceForm.name.trim()) {
      errorMessage.value = '请输入维护窗口名称'
      return false
    }
    if (maintenanceForm.ends_at <= maintenanceForm.starts_at) {
      errorMessage.value = '结束时间必须晚于开始时间'
      return false
    }
    if (maintenanceForm.scope !== 'global' && !maintenanceForm.target_ids.length) {
      errorMessage.value = '请选择维护窗口目标'
      return false
    }
    const payload: AlertMaintenanceInput = {
      ...maintenanceForm,
      name: maintenanceForm.name.trim(),
      reason: maintenanceForm.reason.trim(),
      target_ids: maintenanceForm.scope === 'global' ? [] : [...maintenanceForm.target_ids],
    }
    const id = editingMaintenanceId.value
    const response = await run(
      id ? `maintenance:${id}:save` : 'maintenance:create',
      () => id
        ? updateAlertMaintenance(id, payload, options.request)
        : createAlertMaintenance(payload, options.request),
      id ? '维护窗口已更新' : '维护窗口已创建',
    )
    if (!response) return false
    resetMaintenanceForm()
    await Promise.all([loadMaintenance(), loadSummary(true), loadEvents(true)])
    return true
  }

  async function removeMaintenance(window: AlertMaintenanceWindow) {
    const response = await run(
      `maintenance:${window.id}:delete`,
      async () => {
        await deleteAlertMaintenance(window.id, options.request)
        return true
      },
      '维护窗口已删除',
    )
    if (!response) return false
    if (editingMaintenanceId.value === window.id) resetMaintenanceForm()
    await Promise.all([loadMaintenance(), loadSummary(true), loadEvents(true)])
    return true
  }

  function addHeader() {
    channelForm.replace_headers = true
    channelForm.headers.push({ id: ++headerId, name: '', value: '' })
  }

  function removeHeader(id: number) {
    channelForm.headers = channelForm.headers.filter((header) => header.id !== id)
  }

  function editChannel(channel: AlertWebhookChannel) {
    editingChannelId.value = channel.id
    Object.assign(channelForm, {
      name: channel.name,
      url: '',
      secret: '',
      clear_secret: false,
      replace_headers: false,
      enabled: channel.enabled,
      headers: [],
    })
  }

  function resetChannelForm() {
    editingChannelId.value = ''
    Object.assign(channelForm, {
      name: '',
      url: '',
      secret: '',
      clear_secret: false,
      replace_headers: false,
      enabled: true,
      headers: [],
    })
  }

  async function saveChannel() {
    const id = editingChannelId.value
    if (!channelForm.name.trim()) {
      errorMessage.value = '请输入 Webhook 名称'
      return false
    }
    if (!id && !channelForm.url.trim()) {
      errorMessage.value = '请输入 Webhook URL'
      return false
    }
    const headers: Record<string, string> = {}
    for (const header of channelForm.headers) {
      const name = header.name.trim()
      if (!name) continue
      headers[name] = header.value
    }
    const payload: AlertWebhookChannelInput = {
      name: channelForm.name.trim(),
      enabled: channelForm.enabled,
      clear_secret: channelForm.clear_secret && !channelForm.secret.trim(),
    }
    if (channelForm.url.trim()) payload.url = channelForm.url.trim()
    if (channelForm.secret.trim()) payload.secret = channelForm.secret.trim()
    if (!id || channelForm.replace_headers) payload.headers = headers
    const response = await run(
      id ? `channel:${id}:save` : 'channel:create',
      () => id
        ? updateAlertChannel(id, payload, options.request)
        : createAlertChannel(payload, options.request),
      id ? 'Webhook 已更新' : 'Webhook 已创建',
    )
    if (!response) return false
    resetChannelForm()
    await loadChannels()
    return true
  }

  async function removeChannel(channel: AlertWebhookChannel) {
    const response = await run(
      `channel:${channel.id}:delete`,
      async () => {
        await deleteAlertChannel(channel.id, options.request)
        return true
      },
      'Webhook 已删除',
    )
    if (!response) return false
    if (editingChannelId.value === channel.id) resetChannelForm()
    await Promise.all([loadChannels(), loadRules()])
    return true
  }

  async function testChannel(channel: AlertWebhookChannel) {
    const response = await run(
      `channel:${channel.id}:test`,
      () => testAlertChannel(channel.id, options.request),
      '测试投递已进入队列',
    )
    if (!response) return false
    if (loadedTabs.has('deliveries')) await loadDeliveries()
    return true
  }

  function updateDeliveryQuery(patch: Partial<AlertDeliveryQuery>) {
    Object.assign(deliveryQuery, patch)
    deliveryQuery.page = 1
    void loadDeliveries()
  }

  function resetDeliveryQuery() {
    Object.assign(deliveryQuery, defaultDeliveryQuery())
    void loadDeliveries()
  }

  function setDeliveryPage(page: number) {
    const next = Math.max(1, Math.min(page, deliveries.value.pages || 1))
    if (next === deliveryQuery.page) return
    deliveryQuery.page = next
    void loadDeliveries()
  }

  async function toggleDeliveryDetail(delivery: AlertDelivery) {
    if (expandedDeliveryId.value === delivery.id) {
      expandedDeliveryId.value = ''
      deliveryDetail.value = null
      return
    }
    expandedDeliveryId.value = delivery.id
    deliveryDetail.value = null
    await loadDeliveryDetail(delivery.id)
  }

  async function loadDeliveryDetail(id: string, silent = false) {
    const request = ++deliveryDetailRequest
    const task = async () => {
      const detail = await getAlertDelivery(id, options.request)
      if (request === deliveryDetailRequest && expandedDeliveryId.value === id) {
        deliveryDetail.value = detail
      }
    }
    if (silent) {
      try {
        await task()
      } catch {
        // Detail refresh is best effort.
      }
      return
    }
    await run(`delivery:${id}:load`, task)
  }

  async function retryDelivery(delivery: AlertDelivery) {
    const response = await run(
      `delivery:${delivery.id}:retry`,
      () => retryAlertDelivery(delivery.id, options.request),
      '失败投递已重新排队',
    )
    if (!response) return false
    await loadDeliveries()
    return true
  }

  return {
    activeTab,
    summary,
    attentionCount,
    events,
    rules,
    maintenance,
    channels,
    deliveries,
    eventDetail,
    deliveryDetail,
    expandedEventId,
    expandedDeliveryId,
    acknowledgeNote,
    eventQuery,
    deliveryQuery,
    ruleForm,
    maintenanceForm,
    channelForm,
    editingRuleId,
    editingMaintenanceId,
    editingChannelId,
    loading,
    errorMessage,
    successMessage,
    isBusy,
    loadSummary,
    loadEvents,
    loadRules,
    loadMaintenance,
    loadChannels,
    loadDeliveries,
    loadTab,
    refreshCurrentTab,
    updateEventQuery,
    resetEventQuery,
    setEventPage,
    toggleEventDetail,
    acknowledgeEvent,
    selectRulePreset,
    editRule,
    resetRuleForm,
    saveRule,
    toggleRule,
    removeRule,
    editMaintenance,
    resetMaintenanceForm,
    saveMaintenance,
    removeMaintenance,
    addHeader,
    removeHeader,
    editChannel,
    resetChannelForm,
    saveChannel,
    removeChannel,
    testChannel,
    updateDeliveryQuery,
    resetDeliveryQuery,
    setDeliveryPage,
    toggleDeliveryDetail,
    retryDelivery,
  }
}

export type AlertingConsole = ReturnType<typeof useAlertingConsole>
