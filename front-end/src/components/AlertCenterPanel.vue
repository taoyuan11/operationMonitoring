<script setup lang="ts">
import { computed } from 'vue'
import {
  Activity,
  AlertTriangle,
  BellRing,
  Check,
  CheckCircle2,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  Clock3,
  Edit3,
  Gauge,
  HardDrive,
  History,
  LoaderCircle,
  MemoryStick,
  Plus,
  RefreshCw,
  RotateCcw,
  Search,
  Send,
  ServerOff,
  ShieldAlert,
  Trash2,
  Webhook,
  Wrench,
  X,
} from 'lucide-vue-next'
import { ALERT_RULE_PRESETS, type AlertingConsole } from '../composables/useAlertingConsole'
import type {
  AlertDelivery,
  AlertMaintenanceWindow,
  AlertMetric,
  AlertRule,
  AlertWebhookChannel,
  Instance,
} from '../types/domain'
import { formatTime } from '../utils/format'

const props = defineProps<{
  alerting: AlertingConsole
  instances: Instance[]
}>()

const emit = defineEmits<{
  deleteRule: [rule: AlertRule]
  deleteMaintenance: [window: AlertMaintenanceWindow]
  deleteChannel: [channel: AlertWebhookChannel]
}>()

const {
  activeTab,
  summary,
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
  editMaintenance,
  resetMaintenanceForm,
  saveMaintenance,
  addHeader,
  removeHeader,
  editChannel,
  resetChannelForm,
  saveChannel,
  testChannel,
  updateDeliveryQuery,
  resetDeliveryQuery,
  setDeliveryPage,
  toggleDeliveryDetail,
  retryDelivery,
} = props.alerting

const tabs = [
  { id: 'events' as const, label: '事件', icon: BellRing },
  { id: 'rules' as const, label: '规则', icon: ShieldAlert },
  { id: 'maintenance' as const, label: '维护窗口', icon: Wrench },
  { id: 'webhooks' as const, label: 'Webhook', icon: Webhook },
  { id: 'deliveries' as const, label: '投递记录', icon: History },
]

const metricOptions: Array<{ value: AlertMetric; label: string }> = [
  { value: 'node_offline', label: '节点离线' },
  { value: 'cpu_percent', label: 'CPU 使用率' },
  { value: 'memory_percent', label: '内存使用率' },
  { value: 'disk_percent', label: '磁盘使用率' },
  { value: 'latency_ms', label: '网络延迟' },
]

function metricLabel(metric: string) {
  return metricOptions.find((option) => option.value === metric)?.label || metric
}

function metricIcon(metric: AlertMetric) {
  return {
    node_offline: ServerOff,
    cpu_percent: Gauge,
    memory_percent: MemoryStick,
    disk_percent: HardDrive,
    latency_ms: Activity,
  }[metric]
}

function statusLabel(status: string) {
  return {
    firing: '告警中',
    acknowledged: '已确认',
    resolved: '已恢复',
    pending: '待投递',
    processing: '投递中',
    succeeded: '成功',
    failed: '失败',
    suppressed: '已抑制',
  }[status] || status
}

function deliveryKindLabel(kind: string) {
  return {
    'alert.firing': '触发通知',
    'alert.acknowledged': '确认通知',
    'alert.resolved': '恢复通知',
    'webhook.test': '测试通知',
  }[kind] || kind
}

function timelineLabel(kind: string) {
  return {
    firing: '事件触发',
    acknowledged: '管理员确认',
    resolved: '条件恢复',
    observed: '异常持续',
    suppression_changed: '抑制变化',
  }[kind] || kind
}

function snapshotLabel(snapshot: Record<string, unknown>, fallback: string) {
  for (const key of ['name', 'hostname', 'id']) {
    const value = snapshot[key]
    if (typeof value === 'string' && value) return value
  }
  return fallback
}

function instanceLabel(id: string) {
  const instance = props.instances.find((item) => item.id === id)
  return instance?.name || instance?.hostname || id
}

function channelLabel(id: string) {
  return channels.value.items.find((item) => item.id === id)?.name || id
}

function deliveryChannelLabel(delivery: AlertDelivery) {
  const snapshotName = delivery.channel_snapshot.name
  return typeof snapshotName === 'string' && snapshotName
    ? snapshotName
    : channelLabel(delivery.channel_id)
}

const deliveryChannelOptions = computed(() => {
  const options = new Map(channels.value.items.map((channel) => [channel.id, channel.name]))
  for (const delivery of deliveries.value.items) {
    if (options.has(delivery.channel_id)) continue
    options.set(delivery.channel_id, `${deliveryChannelLabel(delivery)}（历史）`)
  }
  if (deliveryQuery.channel_id && !options.has(deliveryQuery.channel_id)) {
    options.set(deliveryQuery.channel_id, deliveryQuery.channel_id)
  }
  return [...options].map(([id, name]) => ({ id, name }))
})

function retryUnavailableReason(delivery: AlertDelivery) {
  const channel = channels.value.items.find((item) => item.id === delivery.channel_id)
  if (!channel) return 'Webhook 渠道已删除，无法重试'
  return channel.enabled ? '' : 'Webhook 渠道已停用，无法重试'
}

function formatDuration(seconds: number) {
  if (seconds < 60) return `${seconds} 秒`
  if (seconds % 3600 === 0) return `${seconds / 3600} 小时`
  if (seconds % 60 === 0) return `${seconds / 60} 分钟`
  return `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒`
}

function formatMetricValue(metric: string, value: number | null) {
  if (metric === 'node_offline') return value && value > 0 ? '离线' : '在线'
  if (value === null || !Number.isFinite(value)) return '未知'
  return metric === 'latency_ms' ? `${value.toFixed(0)} ms` : `${value.toFixed(1)}%`
}

function formatThreshold(rule: Pick<AlertRule, 'metric' | 'threshold'>) {
  if (rule.metric === 'node_offline') return '连接状态'
  return `>= ${formatMetricValue(rule.metric, rule.threshold)}`
}

function dateInputValue(timestamp: number) {
  if (!timestamp) return ''
  const date = new Date(timestamp * 1000)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

function setMaintenanceTime(key: 'starts_at' | 'ends_at', event: Event) {
  const value = Date.parse((event.target as HTMLInputElement).value)
  if (!Number.isNaN(value)) maintenanceForm[key] = Math.floor(value / 1000)
}

function setEventTime(key: 'from' | 'to', event: Event) {
  const raw = (event.target as HTMLInputElement).value
  const value = Date.parse(raw)
  eventQuery[key] = raw && !Number.isNaN(value) ? Math.floor(value / 1000) : null
}

function changeRuleMetric(event: Event) {
  const metric = (event.target as HTMLSelectElement).value as AlertMetric
  const preset = ALERT_RULE_PRESETS.find((item) => item.metric === metric)
  ruleForm.metric = metric
  ruleForm.threshold = preset?.threshold ?? null
}

function maintenanceState(window: AlertMaintenanceWindow) {
  const now = Date.now() / 1000
  if (!window.enabled) return 'disabled'
  if (window.ends_at <= now) return 'ended'
  if (window.starts_at > now) return 'scheduled'
  return 'active'
}

function maintenanceStateLabel(window: AlertMaintenanceWindow) {
  return {
    disabled: '已停用',
    ended: '已结束',
    scheduled: '待开始',
    active: '进行中',
  }[maintenanceState(window)]
}

function eventRangeLabel() {
  if (!events.value.total) return '0 条'
  const start = (events.value.page - 1) * events.value.page_size + 1
  const end = Math.min(events.value.total, start + events.value.items.length - 1)
  return `${start}-${end} / ${events.value.total}`
}

function deliveryRangeLabel() {
  if (!deliveries.value.total) return '0 条'
  const start = (deliveries.value.page - 1) * deliveries.value.page_size + 1
  const end = Math.min(deliveries.value.total, start + deliveries.value.items.length - 1)
  return `${start}-${end} / ${deliveries.value.total}`
}

function jsonValue(value: unknown) {
  return JSON.stringify(value, null, 2)
}
</script>

<template>
  <section class="management-page alert-center-page">
    <header class="page-header alert-center-heading">
      <div class="page-heading-icon alert"><BellRing :size="22" /></div>
      <div>
        <span class="section-kicker">Incident response</span>
        <h2>告警中心</h2>
        <p>处理监控异常、静默计划和 Webhook 投递。</p>
      </div>
      <button
        class="icon-button"
        type="button"
        title="刷新当前页"
        :disabled="loading"
        @click="refreshCurrentTab"
      >
        <LoaderCircle v-if="loading" class="spin" :size="16" />
        <RefreshCw v-else :size="16" />
      </button>
    </header>

    <div class="alert-summary-strip" aria-label="告警摘要">
      <article class="alert-summary-item firing">
        <AlertTriangle :size="17" />
        <span><strong>{{ summary.firing }}</strong><small>未确认</small></span>
      </article>
      <article class="alert-summary-item acknowledged">
        <Check :size="17" />
        <span><strong>{{ summary.acknowledged }}</strong><small>已确认</small></span>
      </article>
      <article class="alert-summary-item suppressed">
        <ShieldAlert :size="17" />
        <span><strong>{{ summary.suppressed }}</strong><small>当前抑制</small></span>
      </article>
      <article class="alert-summary-item resolved">
        <CheckCircle2 :size="17" />
        <span><strong>{{ summary.resolved_24h }}</strong><small>24h 恢复</small></span>
      </article>
    </div>

    <nav class="alert-tabs" aria-label="告警中心视图">
      <button
        v-for="tab in tabs"
        :key="tab.id"
        :class="{ active: activeTab === tab.id }"
        type="button"
        @click="activeTab = tab.id"
      >
        <component :is="tab.icon" :size="15" />
        <span>{{ tab.label }}</span>
        <em v-if="tab.id === 'events' && summary.firing">{{ summary.firing }}</em>
      </button>
    </nav>

    <Transition name="notice">
      <p v-if="errorMessage" class="notice alert-notice"><AlertTriangle :size="15" />{{ errorMessage }}</p>
      <p v-else-if="successMessage" class="alert-success"><CheckCircle2 :size="15" />{{ successMessage }}</p>
    </Transition>

    <div v-if="activeTab === 'events'" class="alert-tab-content">
      <form class="alert-filter-bar" @submit.prevent="updateEventQuery({})">
        <label>
          <span>状态</span>
          <select v-model="eventQuery.status">
            <option value="">全部状态</option>
            <option value="firing">告警中</option>
            <option value="acknowledged">已确认</option>
            <option value="resolved">已恢复</option>
          </select>
        </label>
        <label>
          <span>级别</span>
          <select v-model="eventQuery.severity">
            <option value="">全部级别</option>
            <option value="critical">严重</option>
            <option value="warning">警告</option>
          </select>
        </label>
        <label>
          <span>指标</span>
          <select v-model="eventQuery.metric">
            <option value="">全部指标</option>
            <option v-for="option in metricOptions" :key="option.value" :value="option.value">{{ option.label }}</option>
          </select>
        </label>
        <label>
          <span>节点</span>
          <select v-model="eventQuery.instance_id">
            <option value="">全部节点</option>
            <option v-for="instance in instances" :key="instance.id" :value="instance.id">{{ instance.name || instance.hostname }}</option>
          </select>
        </label>
        <label>
          <span>抑制</span>
          <select v-model="eventQuery.suppressed">
            <option value="">全部</option>
            <option value="true">仅已抑制</option>
            <option value="false">仅未抑制</option>
          </select>
        </label>
        <label>
          <span>开始时间</span>
          <input type="datetime-local" :value="eventQuery.from ? dateInputValue(eventQuery.from) : ''" @change="setEventTime('from', $event)" />
        </label>
        <label>
          <span>结束时间</span>
          <input type="datetime-local" :value="eventQuery.to ? dateInputValue(eventQuery.to) : ''" @change="setEventTime('to', $event)" />
        </label>
        <label class="alert-search-filter">
          <span>关键词</span>
          <span class="input-shell"><Search :size="14" /><input v-model="eventQuery.search" placeholder="规则、节点或事件 ID" /></span>
        </label>
        <div class="alert-filter-actions">
          <button class="text-button" type="button" @click="resetEventQuery"><RotateCcw :size="14" />重置</button>
          <button class="primary-button" type="submit"><Search :size="14" />查询</button>
        </div>
      </form>

      <div class="alert-data-surface event-surface">
        <div class="alert-event-head">
          <span>事件</span><span>节点 / 当前值</span><span>持续状态</span><span>时间</span><span></span>
        </div>
        <div v-if="!events.items.length && !isBusy('events:load')" class="alert-empty">
          <CheckCircle2 :size="24" /><strong>没有匹配事件</strong><span>调整筛选条件或等待新的监控异常。</span>
        </div>
        <div v-else class="alert-event-list">
          <article v-for="event in events.items" :key="event.id" class="alert-event-row">
            <div class="alert-event-primary">
              <span :class="['alert-severity-mark', event.severity]"><component :is="metricIcon(event.metric)" :size="15" /></span>
              <div>
                <strong>{{ snapshotLabel(event.rule_snapshot, metricLabel(event.metric)) }}</strong>
                <small><span :class="['alert-status', event.status]">{{ statusLabel(event.status) }}</span><span :class="['alert-severity', event.severity]">{{ event.severity === 'critical' ? '严重' : '警告' }}</span></small>
              </div>
            </div>
            <div class="alert-event-node">
              <strong>{{ snapshotLabel(event.node_snapshot, instanceLabel(event.instance_id)) }}</strong>
              <small>{{ metricLabel(event.metric) }} · {{ formatMetricValue(event.metric, event.current_value) }}</small>
            </div>
            <div class="alert-event-state">
              <strong>{{ event.match_count }} 次匹配</strong>
              <small v-if="event.suppressed" class="suppression-label"><ShieldAlert :size="11" />{{ event.suppression_reason || '通知已抑制' }}</small>
              <small v-else>{{ formatDuration(event.duration_seconds) }} 后触发</small>
            </div>
            <div class="alert-event-time">
              <strong>{{ formatTime(event.fired_at) }}</strong>
              <small>更新 {{ formatTime(event.last_observed_at) }}</small>
            </div>
            <button
              class="icon-button"
              type="button"
              title="查看事件详情"
              :aria-expanded="expandedEventId === event.id"
              @click="toggleEventDetail(event)"
            ><ChevronDown :size="15" /></button>

            <div v-if="expandedEventId === event.id" class="alert-event-detail">
              <div v-if="!eventDetail || eventDetail.id !== event.id" class="alert-detail-loading"><LoaderCircle class="spin" :size="18" />加载事件详情</div>
              <template v-else>
                <dl class="alert-detail-grid">
                  <div><dt>事件 ID</dt><dd><code>{{ eventDetail.id }}</code></dd></div>
                  <div><dt>阈值</dt><dd>{{ eventDetail.threshold === null ? '连接断开' : formatMetricValue(eventDetail.metric, eventDetail.threshold) }}</dd></div>
                  <div><dt>首次观察</dt><dd>{{ formatTime(eventDetail.first_observed_at) }}</dd></div>
                  <div><dt>确认人</dt><dd>{{ eventDetail.acknowledged_by || '尚未确认' }}</dd></div>
                  <div><dt>恢复原因</dt><dd>{{ eventDetail.resolution_reason || '尚未恢复' }}</dd></div>
                  <div><dt>抑制原因</dt><dd>{{ eventDetail.suppression_reason || '未抑制' }}</dd></div>
                </dl>

                <form v-if="eventDetail.status === 'firing'" class="acknowledge-form" @submit.prevent="acknowledgeEvent(eventDetail)">
                  <label><span>确认备注 <i>可选</i></span><input v-model="acknowledgeNote" maxlength="2000" placeholder="记录负责人、处置动作或关联工单" /></label>
                  <button class="primary-button" type="submit" :disabled="isBusy(`event:${event.id}:acknowledge`)">
                    <LoaderCircle v-if="isBusy(`event:${event.id}:acknowledge`)" class="spin" :size="14" />
                    <Check v-else :size="14" />确认事件
                  </button>
                </form>

                <div class="alert-detail-columns">
                  <section>
                    <h4>事件时间线</h4>
                    <div v-if="!eventDetail.timeline.length" class="alert-compact-empty">暂无时间线记录</div>
                    <ol v-else class="alert-timeline">
                      <li v-for="item in eventDetail.timeline" :key="item.id">
                        <i></i><div><strong>{{ timelineLabel(item.kind) }}</strong><small>{{ item.actor }} · {{ formatTime(item.created_at) }}</small><p v-if="item.note">{{ item.note }}</p></div>
                      </li>
                    </ol>
                  </section>
                  <section>
                    <h4>关联投递</h4>
                    <div v-if="!eventDetail.deliveries.length" class="alert-compact-empty">该事件没有通知渠道</div>
                    <div v-else class="event-delivery-list">
                      <div v-for="delivery in eventDetail.deliveries" :key="delivery.id">
                        <span :class="['delivery-status', delivery.status]">{{ statusLabel(delivery.status) }}</span>
                        <strong>{{ deliveryKindLabel(delivery.kind) }}</strong>
                        <small>{{ deliveryChannelLabel(delivery) }} · {{ delivery.attempts_count }} 次尝试</small>
                      </div>
                    </div>
                  </section>
                </div>
              </template>
            </div>
          </article>
        </div>
        <div class="alert-pagination">
          <span>{{ eventRangeLabel() }}</span>
          <div>
            <button class="icon-button" type="button" title="上一页" :disabled="events.page <= 1" @click="setEventPage(events.page - 1)"><ChevronLeft :size="15" /></button>
            <strong>{{ events.page }} / {{ events.pages || 1 }}</strong>
            <button class="icon-button" type="button" title="下一页" :disabled="events.page >= (events.pages || 1)" @click="setEventPage(events.page + 1)"><ChevronRight :size="15" /></button>
          </div>
        </div>
      </div>
    </div>

    <div v-else-if="activeTab === 'rules'" class="alert-tab-content alert-config-layout">
      <section class="alert-form-panel">
        <div class="alert-panel-heading">
          <div><h3>{{ editingRuleId ? '编辑规则' : '创建规则' }}</h3><p>异常持续达到设定时间后生成事件。</p></div>
          <button v-if="editingRuleId" class="icon-button" type="button" title="取消编辑" @click="resetRuleForm"><X :size="15" /></button>
        </div>
        <div class="alert-presets" aria-label="快速规则预设">
          <button v-for="preset in ALERT_RULE_PRESETS" :key="preset.metric" type="button" :class="{ active: !editingRuleId && ruleForm.metric === preset.metric }" @click="selectRulePreset(preset.metric)">
            <component :is="metricIcon(preset.metric)" :size="14" /><span>{{ metricLabel(preset.metric) }}</span>
          </button>
        </div>
        <form class="stack-form alert-editor-form" @submit.prevent="saveRule">
          <label><span>规则名称</span><input v-model="ruleForm.name" required maxlength="120" placeholder="例如：核心节点 CPU 持续过高" /></label>
          <div class="alert-form-grid">
            <label><span>监控指标</span><select :value="ruleForm.metric" @change="changeRuleMetric"><option v-for="option in metricOptions" :key="option.value" :value="option.value">{{ option.label }}</option></select></label>
            <label v-if="ruleForm.metric !== 'node_offline'"><span>阈值 {{ ruleForm.metric === 'latency_ms' ? '(ms)' : '(%)' }}</span><input v-model.number="ruleForm.threshold" required type="number" min="0" :max="ruleForm.metric === 'latency_ms' ? undefined : 100" step="0.1" /></label>
            <label><span>持续时间（秒）</span><input v-model.number="ruleForm.duration_seconds" required type="number" min="0" max="31536000" /></label>
          </div>
          <fieldset class="alert-choice-fieldset">
            <legend>严重级别</legend>
            <div class="alert-segmented">
              <button type="button" :class="{ active: ruleForm.severity === 'warning' }" @click="ruleForm.severity = 'warning'">警告</button>
              <button type="button" :class="{ active: ruleForm.severity === 'critical' }" @click="ruleForm.severity = 'critical'">严重</button>
            </div>
          </fieldset>
          <fieldset class="alert-choice-fieldset">
            <legend>作用范围</legend>
            <div class="alert-segmented">
              <button type="button" :class="{ active: ruleForm.scope === 'all' }" @click="ruleForm.scope = 'all'; ruleForm.target_instance_ids = []">全部节点</button>
              <button type="button" :class="{ active: ruleForm.scope === 'specific' }" @click="ruleForm.scope = 'specific'">指定节点</button>
            </div>
          </fieldset>
          <fieldset v-if="ruleForm.scope === 'specific'" class="alert-check-fieldset">
            <legend>目标节点</legend>
            <div class="alert-checkbox-list">
              <label v-for="instance in instances" :key="instance.id"><input v-model="ruleForm.target_instance_ids" type="checkbox" :value="instance.id" /><span>{{ instance.name || instance.hostname }}<small>{{ instance.online ? '在线' : '离线' }}</small></span></label>
            </div>
          </fieldset>
          <fieldset class="alert-check-fieldset">
            <legend>通知渠道 <i>可选</i></legend>
            <div v-if="!channels.items.length" class="alert-compact-empty">未配置 Webhook，事件仍会出现在控制台</div>
            <div v-else class="alert-checkbox-list">
              <label v-for="channel in channels.items" :key="channel.id"><input v-model="ruleForm.channel_ids" type="checkbox" :value="channel.id" /><span>{{ channel.name }}<small>{{ channel.enabled ? '已启用' : '已停用' }}</small></span></label>
            </div>
          </fieldset>
          <label class="alert-toggle-row"><input v-model="ruleForm.enabled" type="checkbox" /><span><strong>创建后启用</strong><small>停用规则会自动恢复其活动事件</small></span></label>
          <button class="primary-button" type="submit" :disabled="isBusy(editingRuleId ? `rule:${editingRuleId}:save` : 'rule:create')"><Check :size="15" />{{ editingRuleId ? '保存规则' : '创建规则' }}</button>
        </form>
      </section>

      <section class="alert-list-panel">
        <div class="alert-panel-heading"><div><h3>告警规则</h3><p>{{ rules.total }} 条规则，不会自动创建默认规则。</p></div></div>
        <div v-if="!rules.items.length" class="alert-empty"><ShieldAlert :size="24" /><strong>尚未配置规则</strong><span>从左侧预设开始创建第一条规则。</span></div>
        <div v-else class="alert-rule-list">
          <article v-for="rule in rules.items" :key="rule.id" :class="{ disabled: !rule.enabled, editing: editingRuleId === rule.id }">
            <span :class="['alert-severity-mark', rule.severity]"><component :is="metricIcon(rule.metric)" :size="16" /></span>
            <div class="alert-rule-main"><strong>{{ rule.name }}</strong><small>{{ metricLabel(rule.metric) }} {{ formatThreshold(rule) }} · 持续 {{ formatDuration(rule.duration_seconds) }}</small><p>{{ rule.scope === 'all' ? '全部节点' : `${rule.target_instance_ids.length} 个指定节点` }} · {{ rule.channel_ids.length ? `${rule.channel_ids.length} 个通知渠道` : '仅控制台事件' }}</p></div>
            <label class="switch-control" :title="rule.enabled ? '停用规则' : '启用规则'"><input type="checkbox" :checked="rule.enabled" :disabled="isBusy(`rule:${rule.id}:enabled`)" @change="toggleRule(rule, ($event.target as HTMLInputElement).checked)" /><span></span></label>
            <div class="row-actions">
              <button class="icon-button" type="button" title="编辑规则" @click="editRule(rule)"><Edit3 :size="14" /></button>
              <button class="icon-button danger" type="button" title="删除规则" @click="emit('deleteRule', rule)"><Trash2 :size="14" /></button>
            </div>
          </article>
        </div>
      </section>
    </div>

    <div v-else-if="activeTab === 'maintenance'" class="alert-tab-content alert-config-layout">
      <section class="alert-form-panel">
        <div class="alert-panel-heading"><div><h3>{{ editingMaintenanceId ? '编辑维护窗口' : '创建维护窗口' }}</h3><p>窗口采用开始时间包含、结束时间不包含的区间。</p></div><button v-if="editingMaintenanceId" class="icon-button" type="button" title="取消编辑" @click="resetMaintenanceForm"><X :size="15" /></button></div>
        <form class="stack-form alert-editor-form" @submit.prevent="saveMaintenance">
          <label><span>窗口名称</span><input v-model="maintenanceForm.name" required maxlength="120" placeholder="例如：数据库计划升级" /></label>
          <label><span>维护原因 <i>可选</i></span><textarea v-model="maintenanceForm.reason" maxlength="2000" placeholder="变更内容、负责人或工单编号"></textarea></label>
          <fieldset class="alert-choice-fieldset"><legend>作用范围</legend><div class="alert-segmented"><button type="button" :class="{ active: maintenanceForm.scope === 'global' }" @click="maintenanceForm.scope = 'global'; maintenanceForm.target_ids = []">全局</button><button type="button" :class="{ active: maintenanceForm.scope === 'rule' }" @click="maintenanceForm.scope = 'rule'; maintenanceForm.target_ids = []">指定规则</button><button type="button" :class="{ active: maintenanceForm.scope === 'node' }" @click="maintenanceForm.scope = 'node'; maintenanceForm.target_ids = []">指定节点</button></div></fieldset>
          <fieldset v-if="maintenanceForm.scope !== 'global'" class="alert-check-fieldset">
            <legend>{{ maintenanceForm.scope === 'rule' ? '目标规则' : '目标节点' }}</legend>
            <div class="alert-checkbox-list">
              <label v-for="item in maintenanceForm.scope === 'rule' ? rules.items : instances" :key="item.id"><input v-model="maintenanceForm.target_ids" type="checkbox" :value="item.id" /><span>{{ 'metric' in item ? item.name : (item.name || item.hostname) }}</span></label>
            </div>
          </fieldset>
          <div class="alert-form-grid">
            <label><span>开始时间</span><input required type="datetime-local" :value="dateInputValue(maintenanceForm.starts_at)" @change="setMaintenanceTime('starts_at', $event)" /></label>
            <label><span>结束时间</span><input required type="datetime-local" :value="dateInputValue(maintenanceForm.ends_at)" @change="setMaintenanceTime('ends_at', $event)" /></label>
          </div>
          <label class="alert-toggle-row"><input v-model="maintenanceForm.enabled" type="checkbox" /><span><strong>启用窗口</strong><small>窗口内仍生成事件，只抑制首次触发通知</small></span></label>
          <button class="primary-button" type="submit"><Check :size="15" />{{ editingMaintenanceId ? '保存窗口' : '创建窗口' }}</button>
        </form>
      </section>
      <section class="alert-list-panel">
        <div class="alert-panel-heading"><div><h3>维护计划</h3><p>{{ maintenance.total }} 个一次性维护窗口。</p></div></div>
        <div v-if="!maintenance.items.length" class="alert-empty"><Clock3 :size="24" /><strong>没有维护计划</strong><span>需要静默通知时在左侧创建窗口。</span></div>
        <div v-else class="maintenance-list">
          <article v-for="window in maintenance.items" :key="window.id" :class="maintenanceState(window)">
            <div class="maintenance-status"><Clock3 :size="16" /><span :class="['alert-status', maintenanceState(window)]">{{ maintenanceStateLabel(window) }}</span></div>
            <div><strong>{{ window.name }}</strong><small>{{ formatTime(window.starts_at) }} - {{ formatTime(window.ends_at) }}</small><p>{{ window.scope === 'global' ? '全局' : window.scope === 'rule' ? `${window.target_ids.length} 条规则` : `${window.target_ids.length} 个节点` }}<template v-if="window.reason"> · {{ window.reason }}</template></p></div>
            <div class="row-actions"><button class="icon-button" type="button" title="编辑维护窗口" @click="editMaintenance(window)"><Edit3 :size="14" /></button><button class="icon-button danger" type="button" title="删除维护窗口" @click="emit('deleteMaintenance', window)"><Trash2 :size="14" /></button></div>
          </article>
        </div>
      </section>
    </div>

    <div v-else-if="activeTab === 'webhooks'" class="alert-tab-content alert-config-layout">
      <section class="alert-form-panel">
        <div class="alert-panel-heading"><div><h3>{{ editingChannelId ? '编辑 Webhook' : '创建 Webhook' }}</h3><p>投递支持 HMAC-SHA256 签名和自定义请求头。</p></div><button v-if="editingChannelId" class="icon-button" type="button" title="取消编辑" @click="resetChannelForm"><X :size="15" /></button></div>
        <form class="stack-form alert-editor-form" @submit.prevent="saveChannel">
          <label><span>渠道名称</span><input v-model="channelForm.name" required maxlength="120" placeholder="例如：值班系统" /></label>
          <label><span>Webhook URL <i v-if="editingChannelId">留空保留现有值</i></span><input v-model="channelForm.url" :required="!editingChannelId" type="url" placeholder="https://hooks.internal.example/alerts" /></label>
          <label><span>HMAC 密钥 <i>{{ editingChannelId ? '留空保留现有值' : '可选' }}</i></span><input v-model="channelForm.secret" type="password" autocomplete="new-password" placeholder="用于校验 X-OM-Signature" /></label>
          <label v-if="editingChannelId" class="alert-check-line"><input v-model="channelForm.clear_secret" type="checkbox" :disabled="Boolean(channelForm.secret)" /><span>清除现有 HMAC 密钥</span></label>
          <fieldset class="alert-check-fieldset webhook-header-fieldset">
            <legend>自定义请求头 <i>最多 32 个</i></legend>
            <label v-if="editingChannelId" class="alert-check-line"><input v-model="channelForm.replace_headers" type="checkbox" /><span>替换现有请求头（关闭时保留）</span></label>
            <div v-if="channelForm.replace_headers || !editingChannelId" class="webhook-header-editor">
              <div v-for="header in channelForm.headers" :key="header.id" class="webhook-header-row"><input v-model="header.name" maxlength="128" placeholder="Header-Name" /><input v-model="header.value" maxlength="4096" placeholder="值" /><button class="icon-button danger" type="button" title="移除请求头" @click="removeHeader(header.id)"><X :size="14" /></button></div>
              <button class="text-button" type="button" :disabled="channelForm.headers.length >= 32" @click="addHeader"><Plus :size="14" />添加请求头</button>
            </div>
          </fieldset>
          <label class="alert-toggle-row"><input v-model="channelForm.enabled" type="checkbox" /><span><strong>启用渠道</strong><small>停用后不会接收新的告警投递</small></span></label>
          <button class="primary-button" type="submit"><Check :size="15" />{{ editingChannelId ? '保存 Webhook' : '创建 Webhook' }}</button>
        </form>
      </section>
      <section class="alert-list-panel">
        <div class="alert-panel-heading"><div><h3>Webhook 渠道</h3><p>URL 与密钥仅以脱敏状态显示。</p></div></div>
        <div v-if="!channels.items.length" class="alert-empty"><Webhook :size="24" /><strong>尚未配置 Webhook</strong><span>规则可以不绑定渠道，仅在控制台形成闭环。</span></div>
        <div v-else class="webhook-channel-list">
          <article v-for="channel in channels.items" :key="channel.id" :class="{ disabled: !channel.enabled }">
            <span class="webhook-channel-icon"><Webhook :size="17" /></span>
            <div><strong>{{ channel.name }}</strong><code>{{ channel.masked_url }}</code><small>{{ channel.has_secret ? '已配置签名密钥' : '未配置签名密钥' }} · {{ channel.header_names.length ? `${channel.header_names.length} 个请求头：${channel.header_names.join('、')}` : '无自定义请求头' }}</small></div>
            <span :class="['alert-status', channel.enabled ? 'succeeded' : 'disabled']">{{ channel.enabled ? '已启用' : '已停用' }}</span>
            <div class="row-actions"><button class="text-button" type="button" :disabled="!channel.enabled || isBusy(`channel:${channel.id}:test`)" @click="testChannel(channel)"><Send :size="13" />测试</button><button class="icon-button" type="button" title="编辑 Webhook" @click="editChannel(channel)"><Edit3 :size="14" /></button><button class="icon-button danger" type="button" title="删除 Webhook" @click="emit('deleteChannel', channel)"><Trash2 :size="14" /></button></div>
          </article>
        </div>
      </section>
    </div>

    <div v-else class="alert-tab-content">
      <form class="alert-filter-bar delivery-filter-bar" @submit.prevent="updateDeliveryQuery({})">
        <label><span>状态</span><select v-model="deliveryQuery.status"><option value="">全部状态</option><option value="pending">待投递</option><option value="processing">投递中</option><option value="succeeded">成功</option><option value="failed">失败</option><option value="suppressed">已抑制</option></select></label>
        <label><span>类型</span><select v-model="deliveryQuery.kind"><option value="">全部类型</option><option value="alert.firing">触发通知</option><option value="alert.acknowledged">确认通知</option><option value="alert.resolved">恢复通知</option><option value="webhook.test">测试通知</option></select></label>
        <label><span>渠道</span><select v-model="deliveryQuery.channel_id"><option value="">全部渠道</option><option v-for="channel in deliveryChannelOptions" :key="channel.id" :value="channel.id">{{ channel.name }}</option></select></label>
        <label class="alert-search-filter"><span>事件 ID</span><input v-model="deliveryQuery.event_id" placeholder="精确筛选关联事件" /></label>
        <div class="alert-filter-actions"><button class="text-button" type="button" @click="resetDeliveryQuery"><RotateCcw :size="14" />重置</button><button class="primary-button" type="submit"><Search :size="14" />查询</button></div>
      </form>
      <div class="alert-data-surface delivery-surface">
        <div class="alert-delivery-head"><span>投递</span><span>渠道</span><span>尝试</span><span>更新时间</span><span></span></div>
        <div v-if="!deliveries.items.length && !isBusy('deliveries:load')" class="alert-empty"><Send :size="24" /><strong>没有匹配投递</strong><span>测试 Webhook 或触发绑定渠道的事件后会显示记录。</span></div>
        <div v-else class="alert-delivery-list">
          <article v-for="delivery in deliveries.items" :key="delivery.id" class="alert-delivery-row">
            <div><span :class="['delivery-status', delivery.status]">{{ statusLabel(delivery.status) }}</span><strong>{{ deliveryKindLabel(delivery.kind) }}</strong><small>{{ delivery.event_id ? `事件 ${delivery.event_id.slice(0, 8)}` : '独立测试' }}</small></div>
            <div><strong>{{ deliveryChannelLabel(delivery) }}</strong><small>{{ delivery.channel_id.slice(0, 12) }}</small></div>
            <div><strong>{{ delivery.attempts_count }} 次</strong><small v-if="delivery.status === 'failed'">{{ delivery.last_error || '等待手动重试' }}</small><small v-else-if="delivery.next_attempt_at">下次 {{ formatTime(delivery.next_attempt_at) }}</small><small v-else>{{ delivery.manual_retry_count }} 次手动重试</small></div>
            <div><strong>{{ formatTime(delivery.updated_at) }}</strong><small v-if="delivery.completed_at">完成 {{ formatTime(delivery.completed_at) }}</small></div>
            <div class="row-actions"><button v-if="delivery.status === 'failed'" class="icon-button" type="button" :title="retryUnavailableReason(delivery) || '重新投递'" :disabled="Boolean(retryUnavailableReason(delivery)) || isBusy(`delivery:${delivery.id}:retry`)" @click="retryDelivery(delivery)"><RefreshCw :size="14" /></button><button class="icon-button" type="button" title="查看投递详情" :aria-expanded="expandedDeliveryId === delivery.id" @click="toggleDeliveryDetail(delivery)"><ChevronDown :size="15" /></button></div>
            <div v-if="expandedDeliveryId === delivery.id" class="alert-delivery-detail">
              <div v-if="!deliveryDetail || deliveryDetail.id !== delivery.id" class="alert-detail-loading"><LoaderCircle class="spin" :size="18" />加载投递详情</div>
              <template v-else>
                <dl class="alert-detail-grid"><div><dt>投递 ID</dt><dd><code>{{ deliveryDetail.id }}</code></dd></div><div><dt>抑制原因</dt><dd>{{ deliveryDetail.suppression_reason || '未抑制' }}</dd></div><div><dt>最后错误</dt><dd>{{ deliveryDetail.last_error || '无' }}</dd></div></dl>
                <div class="alert-detail-columns">
                  <section><h4>尝试记录</h4><div v-if="!deliveryDetail.attempts.length" class="alert-compact-empty">尚未发起 HTTP 请求</div><div v-else class="delivery-attempt-list"><div v-for="attempt in deliveryDetail.attempts" :key="attempt.id"><span>#{{ attempt.attempt_number }}</span><strong>{{ attempt.http_status || '网络错误' }} · {{ attempt.duration_ms }} ms</strong><small>{{ formatTime(attempt.created_at) }}<template v-if="attempt.error"> · {{ attempt.error }}</template></small><pre v-if="attempt.response_excerpt">{{ attempt.response_excerpt }}</pre></div></div></section>
                  <section><h4>投递负载</h4><pre class="delivery-payload">{{ jsonValue(deliveryDetail.payload) }}</pre></section>
                </div>
              </template>
            </div>
          </article>
        </div>
        <div class="alert-pagination"><span>{{ deliveryRangeLabel() }}</span><div><button class="icon-button" type="button" title="上一页" :disabled="deliveries.page <= 1" @click="setDeliveryPage(deliveries.page - 1)"><ChevronLeft :size="15" /></button><strong>{{ deliveries.page }} / {{ deliveries.pages || 1 }}</strong><button class="icon-button" type="button" title="下一页" :disabled="deliveries.page >= (deliveries.pages || 1)" @click="setDeliveryPage(deliveries.page + 1)"><ChevronRight :size="15" /></button></div></div>
      </div>
    </div>
  </section>
</template>
