<script setup lang="ts">
import { computed, ref } from 'vue'
import {
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ClipboardList,
  Download,
  Image,
  ListChecks,
  LoaderCircle,
  Monitor,
  Moon,
  Palette,
  Plus,
  RefreshCw,
  Search,
  Settings,
  Sun,
  Terminal,
  Trash2,
  X,
} from 'lucide-vue-next'
import type {
  AdminTab,
  AuditEvent,
  AuditExportFormat,
  AuditPage,
  AuditQuery,
  CommandJob,
  CommandRecord,
  PendingInstance,
  ResolvedTheme,
  ThemeMode,
} from '../types/domain'
import { formatTime } from '../utils/format'

const props = defineProps<{
  adminTab: AdminTab
  pendingInstances: PendingInstance[]
  commands: CommandRecord[]
  jobs: CommandJob[]
  audit: AuditPage
  auditQuery: AuditQuery
  auditLoading: boolean
  auditError: string
  auditExporting: AuditExportFormat | null
  settingsForm: {
    retention_days: number
    audit_retention_days: number
    alert_retention_days: number
    background_image_url: string | null
    theme_mode: ThemeMode
    accent_color: string
  }
  resolvedTheme: ResolvedTheme
  appearanceMessage: string
  backgroundFileName: string
  backgroundOperation: 'uploading' | 'removing' | null
  backgroundMessage: string
  commandForm: {
    name: string
    command: string
    confirm_text: string
  }
}>()

const themeOptions = [
  { value: 'auto' as const, label: '跟随系统', description: '自动使用设备的明暗外观', icon: Monitor },
  { value: 'light' as const, label: '明亮', description: '始终使用浅色界面', icon: Sun },
  { value: 'dark' as const, label: '暗黑', description: '始终使用深色界面', icon: Moon },
]

const accentPresets = [
  { label: '翠绿', value: '#3bbf9b' },
  { label: '海蓝', value: '#3b82f6' },
  { label: '青蓝', value: '#159eaa' },
  { label: '紫罗兰', value: '#8b5cf6' },
  { label: '琥珀', value: '#d08a24' },
  { label: '玫红', value: '#d95f80' },
]

const previewTheme = computed(() =>
  props.settingsForm.theme_mode === 'auto' ? props.resolvedTheme : props.settingsForm.theme_mode,
)

const expandedAuditId = ref('')
const auditRange = computed(() => {
  if (props.audit.total === 0) return '0 条记录'
  if (props.audit.items.length === 0) return `0 / ${props.audit.total}`
  const start = (props.audit.page - 1) * props.audit.page_size + 1
  const end = Math.min(props.audit.total, start + props.audit.items.length - 1)
  return `${start}-${end} / ${props.audit.total}`
})

function dateInputValue(timestamp: number | null, endOfMinute = false) {
  if (!timestamp) return ''
  const date = new Date((timestamp - (endOfMinute ? 60 : 0)) * 1000)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

function timestampFromInput(value: string, endOfMinute = false) {
  if (!value) return null
  const timestamp = Date.parse(value)
  return Number.isNaN(timestamp) ? null : Math.floor(timestamp / 1000) + (endOfMinute ? 60 : 0)
}

function updateDateFilter(key: 'from' | 'to', event: Event) {
  const value = (event.target as HTMLInputElement).value
  const patch = { [key]: timestampFromInput(value, key === 'to') } as Partial<AuditQuery>
  emit('auditQueryChanged', patch)
}

type AuditTextFilter = 'user_id' | 'actor' | 'category' | 'action' | 'instance_id' | 'source_ip' | 'request_id' | 'keyword'

function updateTextFilter(key: AuditTextFilter, event: Event) {
  emit('auditQueryChanged', { [key]: (event.target as HTMLInputElement).value })
}

function updateStatusFilter(event: Event) {
  emit('auditQueryChanged', { status: (event.target as HTMLSelectElement).value as AuditQuery['status'] })
}

function toggleAuditDetails(event: AuditEvent) {
  expandedAuditId.value = expandedAuditId.value === event.id ? '' : event.id
}

function statusLabel(status: AuditEvent['status']) {
  return {
    running: '进行中',
    success: '成功',
    partial_success: '部分成功',
    failed: '失败',
    cancelled: '已取消',
  }[status] || status
}

function categoryLabel(category: string) {
  return {
    admin: '管理',
    auth: '认证',
    session: '会话',
    terminal: '终端',
    desktop: '桌面',
    docker: 'Docker',
    security: '安全',
  }[category] || category || '其他'
}

function nodeLabel(event: AuditEvent) {
  const snapshot = event.node_snapshot
  for (const key of ['name', 'hostname', 'id']) {
    const value = snapshot?.[key]
    if (typeof value === 'string' && value) return value
  }
  return event.instance_id || '全局'
}

function jsonValue(value: unknown) {
  if (value === null || value === undefined) return ''
  return JSON.stringify(value, null, 2)
}

function hasJsonContent(value: Record<string, unknown> | null) {
  return value !== null && Object.keys(value).length > 0
}

const emit = defineEmits<{
  approve: [id: string]
  reject: [id: string]
  createCommand: []
  removeCommand: [command: CommandRecord]
  saveSettings: []
  saveAppearance: []
  appearanceChanged: []
  selectBackgroundImage: [event: Event]
  clearBackgroundImage: []
  auditQueryChanged: [patch: Partial<AuditQuery>]
  auditPageChanged: [page: number]
  refreshAudit: []
  exportAudit: [format: AuditExportFormat]
}>()
</script>

<template>
  <section class="management-page">
    <template v-if="adminTab === 'pending'">
      <header class="page-header">
        <div class="page-heading-icon"><ListChecks :size="22" /></div>
        <div>
          <span class="section-kicker">Access review</span>
          <h2>接入审核</h2>
          <p>审核新节点的接入请求，仅批准可信设备。</p>
        </div>
        <span class="page-count">{{ pendingInstances.length }} 个待处理</span>
      </header>

      <div class="admin-content-card wide-card admin-list-card">
        <div class="card-heading">
          <div><h3>待审批节点</h3><p>节点在获得批准之前不会出现在公开实例列表中。</p></div>
        </div>
        <Transition name="content" mode="out-in">
          <div v-if="pendingInstances.length === 0" key="empty" class="page-empty">
            <span><Check :size="24" /></span>
            <strong>所有申请均已处理</strong>
            <p>目前没有等待审核的新节点。</p>
          </div>
          <TransitionGroup v-else key="approvals" name="row" tag="div" class="approval-list">
            <article v-for="item in pendingInstances" :key="item.id" class="approval-row">
              <span class="list-icon"><Terminal :size="17" /></span>
              <div class="approval-identity">
                <strong>{{ item.hostname }}</strong>
                <span>{{ item.os }}/{{ item.arch }} · Agent {{ item.agent_version }}</span>
              </div>
              <div class="approval-time">
                <span>最后请求</span>
                <strong>{{ formatTime(item.last_seen) }}</strong>
              </div>
              <div class="row-actions">
                <button class="approve-button" type="button" @click="$emit('approve', item.id)">
                  <Check :size="15" />批准
                </button>
                <button class="reject-button" type="button" @click="$emit('reject', item.id)">
                  <X :size="15" />拒绝
                </button>
              </div>
            </article>
          </TransitionGroup>
        </Transition>
      </div>
    </template>

    <template v-if="adminTab === 'commands'">
      <header class="page-header">
        <div class="page-heading-icon purple"><Terminal :size="22" /></div>
        <div>
          <span class="section-kicker">Remote actions</span>
          <h2>快捷命令</h2>
          <p>维护可在实例节点上安全执行的预设操作。</p>
        </div>
        <span class="page-count">{{ commands.length }} 个已启用</span>
      </header>

      <div class="admin-page-grid commands-layout">
        <div class="admin-content-card">
          <div class="card-heading"><div><h3>创建快捷命令</h3><p>添加后可直接在主页节点卡片中执行。</p></div></div>
          <form class="stack-form page-form" @submit.prevent="$emit('createCommand')">
            <label><span>显示名称</span><input v-model="commandForm.name" required placeholder="例如：重启 Nginx" /></label>
            <label><span>执行命令</span><input v-model="commandForm.command" required placeholder="systemctl restart nginx" /></label>
            <label><span>确认提示 <i>可选</i></span><input v-model="commandForm.confirm_text" placeholder="执行前显示的二次确认提示" /></label>
            <button class="primary-button" type="submit"><Plus :size="16" />添加快捷命令</button>
          </form>
        </div>

        <div class="admin-content-card">
          <div class="card-heading"><div><h3>已启用命令</h3><p>当前允许执行的命令白名单。</p></div></div>
          <Transition name="content" mode="out-in">
            <div v-if="commands.length === 0" key="empty" class="compact-empty">暂无快捷命令</div>
            <TransitionGroup v-else key="commands" name="row" tag="div" class="command-list">
              <article v-for="command in commands" :key="command.id" class="command-row">
                <span class="list-icon"><Terminal :size="16" /></span>
                <div><strong>{{ command.name }}</strong><code>{{ command.command }}</code></div>
                <button class="icon-button danger" type="button" title="停用" @click="$emit('removeCommand', command)">
                  <Trash2 :size="15" />
                </button>
              </article>
            </TransitionGroup>
          </Transition>
        </div>

        <div class="admin-content-card recent-jobs-card">
          <div class="card-heading"><div><h3>最近执行记录</h3><p>最近提交到节点的命令任务。</p></div></div>
          <Transition name="content" mode="out-in">
            <div v-if="jobs.length === 0" key="empty" class="compact-empty">暂无执行记录</div>
            <TransitionGroup v-else key="jobs" name="row" tag="div" class="jobs-table">
              <article v-for="job in jobs.slice(0, 10)" :key="job.id" class="job-table-row">
                <span :class="['job-status', job.status]">{{ job.status }}</span>
                <strong>{{ job.command }}</strong>
                <small>节点 {{ job.instance_id.slice(0, 8) }}</small>
                <time>{{ formatTime(job.created_at) }}</time>
              </article>
            </TransitionGroup>
          </Transition>
        </div>
      </div>
    </template>

    <template v-if="adminTab === 'settings'">
      <header class="page-header">
        <div class="page-heading-icon amber"><Settings :size="22" /></div>
        <div>
          <span class="section-kicker">System preferences</span>
          <h2>系统设置</h2>
          <p>管理数据生命周期与监控页面的视觉外观。</p>
        </div>
      </header>

      <div class="admin-page-grid settings-layout">
        <div class="admin-content-card appearance-settings-card">
          <div class="card-heading">
            <div><h3>外观主题</h3><p>设置所有访问者看到的界面明暗模式与主要强调色。</p></div>
            <span class="appearance-current">当前显示：{{ resolvedTheme === 'light' ? '明亮' : '暗黑' }}</span>
          </div>

          <div class="appearance-settings-body">
            <div class="appearance-controls">
              <fieldset class="theme-mode-fieldset">
                <legend>主题模式</legend>
                <div class="theme-mode-options">
                  <button
                    v-for="option in themeOptions"
                    :key="option.value"
                    :class="['theme-mode-option', { active: settingsForm.theme_mode === option.value }]"
                    type="button"
                    :aria-pressed="settingsForm.theme_mode === option.value"
                    @click="settingsForm.theme_mode = option.value; $emit('appearanceChanged')"
                  >
                    <component :is="option.icon" :size="18" />
                    <span><strong>{{ option.label }}</strong><small>{{ option.description }}</small></span>
                    <i aria-hidden="true"></i>
                  </button>
                </div>
              </fieldset>

              <fieldset class="accent-fieldset">
                <legend>主题色</legend>
                <div class="accent-presets">
                  <button
                    v-for="preset in accentPresets"
                    :key="preset.value"
                    :class="['accent-swatch', { active: settingsForm.accent_color === preset.value }]"
                    :style="{ '--swatch-color': preset.value }"
                    type="button"
                    :title="preset.label"
                    :aria-label="`使用${preset.label}主题色`"
                    :aria-pressed="settingsForm.accent_color === preset.value"
                    @click="settingsForm.accent_color = preset.value; $emit('appearanceChanged')"
                  ><Check v-if="settingsForm.accent_color === preset.value" :size="14" /></button>
                  <label class="custom-color-picker">
                    <Palette :size="16" />
                    <span>自定义</span>
                    <input v-model="settingsForm.accent_color" type="color" aria-label="选择自定义主题色" @input="$emit('appearanceChanged')" />
                    <code>{{ settingsForm.accent_color }}</code>
                  </label>
                </div>
              </fieldset>

              <div class="appearance-save-row">
                <button class="primary-button" type="button" @click="$emit('saveAppearance')">
                  <Palette :size="16" />保存外观
                </button>
                <small :class="{ success: appearanceMessage }">{{ appearanceMessage || '保存后将作为全站默认外观' }}</small>
              </div>
            </div>

            <div
              :class="['appearance-preview', `preview-${previewTheme}`]"
              :style="{ '--preview-accent': settingsForm.accent_color }"
            >
              <span class="preview-label">实时预览</span>
              <div class="preview-window">
                <header><i></i><i></i><i></i></header>
                <section>
                  <div class="preview-sidebar"><span></span><span class="active"></span><span></span></div>
                  <div class="preview-content">
                    <div class="preview-summary"><i></i><i></i><i></i></div>
                    <div class="preview-card"><strong>运行监控</strong><span></span><button type="button">主要操作</button></div>
                  </div>
                </section>
              </div>
              <small>{{ previewTheme === 'light' ? '明亮主题预览' : '暗黑主题预览' }}</small>
            </div>
          </div>
        </div>

        <div class="admin-content-card background-card">
          <div class="card-heading"><div><h3>页面背景</h3><p>自定义监控页面背景，系统会自动添加深色遮罩。</p></div></div>
          <div :class="['large-background-preview', { empty: !settingsForm.background_image_url }]">
            <Transition name="fade" mode="out-in">
              <img
                v-if="settingsForm.background_image_url"
                key="image"
                :src="settingsForm.background_image_url"
                alt="当前背景"
              />
              <div v-else key="empty"><Image :size="28" /><span>当前使用默认渐变背景</span></div>
            </Transition>
          </div>
          <div class="background-toolbar">
            <label :class="['file-button', { disabled: backgroundOperation }]">
              <LoaderCircle v-if="backgroundOperation === 'uploading'" class="spin" :size="15" />
              <Image v-else :size="15" />
              {{ backgroundOperation === 'uploading' ? '正在上传' : '选择图片' }}
              <input
                type="file"
                accept="image/png,image/jpeg,image/webp"
                :disabled="Boolean(backgroundOperation)"
                @change="$emit('selectBackgroundImage', $event)"
              />
            </label>
            <Transition name="fade-scale">
              <button
                v-if="settingsForm.background_image_url"
                class="text-button danger"
                type="button"
                :disabled="Boolean(backgroundOperation)"
                @click="$emit('clearBackgroundImage')"
              >
                <LoaderCircle v-if="backgroundOperation === 'removing'" class="spin" :size="14" />
                <Trash2 v-else :size="14" />
                {{ backgroundOperation === 'removing' ? '正在移除' : '移除背景' }}
              </button>
            </Transition>
            <small :class="{ success: backgroundMessage }">{{ backgroundMessage || backgroundFileName || '支持 PNG、JPEG、WebP，最大 5MB' }}</small>
          </div>
        </div>

        <div class="admin-content-card retention-card">
          <div class="card-heading"><div><h3>数据保留</h3><p>分别设置历史指标、审计事件和已恢复告警的自动清理期限。</p></div></div>
          <form class="stack-form page-form" @submit.prevent="$emit('saveSettings')">
            <label>
              <span>指标保留天数</span>
              <input v-model.number="settingsForm.retention_days" min="1" max="365" type="number" />
            </label>
            <small class="form-help">可设置 1 至 365 天，不影响节点基础信息。</small>
            <label>
              <span>审计保留天数</span>
              <input v-model.number="settingsForm.audit_retention_days" min="1" max="3650" type="number" />
            </label>
            <small class="form-help">可设置 1 至 3650 天，运行中的会话不会被清理。</small>
            <label>
              <span>告警保留天数</span>
              <input v-model.number="settingsForm.alert_retention_days" min="1" max="3650" type="number" />
            </label>
            <small class="form-help">仅清理过期的已恢复事件及投递记录，活动事件不会被清理。</small>
            <button class="primary-button" type="submit"><Settings :size="16" />保存设置</button>
          </form>
        </div>
      </div>
    </template>

    <template v-if="adminTab === 'logs'">
      <header class="page-header">
        <div class="page-heading-icon cyan"><ClipboardList :size="22" /></div>
        <div>
          <span class="section-kicker">Audit trail</span>
          <h2>统一审计</h2>
          <p>查询管理员操作、认证安全事件与远程会话生命周期。</p>
        </div>
        <span class="page-count">{{ auditRange }}</span>
      </header>

      <div class="admin-content-card wide-card admin-list-card" :aria-busy="auditLoading">
        <div class="card-heading audit-heading">
          <div><h3>审计记录</h3><p>按创建时间倒序排列，支持节点、用户、请求和状态筛选。</p></div>
          <div class="audit-toolbar-actions">
            <button class="icon-button subtle" type="button" title="刷新审计记录" :disabled="auditLoading" @click="$emit('refreshAudit')">
              <RefreshCw :class="{ spin: auditLoading }" :size="15" />
            </button>
            <button class="text-button" type="button" :disabled="Boolean(auditExporting)" @click="$emit('exportAudit', 'csv')">
              <LoaderCircle v-if="auditExporting === 'csv'" class="spin" :size="14" /><Download v-else :size="14" />CSV
            </button>
            <button class="text-button" type="button" :disabled="Boolean(auditExporting)" @click="$emit('exportAudit', 'json')">
              <LoaderCircle v-if="auditExporting === 'json'" class="spin" :size="14" /><Download v-else :size="14" />JSON
            </button>
          </div>
        </div>

        <div class="audit-filters">
          <label class="audit-filter-wide"><span>关键字</span><div class="input-shell"><Search :size="15" /><input :value="auditQuery.keyword" placeholder="动作、目标或详情" @change="updateTextFilter('keyword', $event)" /></div></label>
          <label><span>开始时间</span><input :value="dateInputValue(auditQuery.from)" type="datetime-local" @change="updateDateFilter('from', $event)" /></label>
          <label><span>结束时间</span><input :value="dateInputValue(auditQuery.to, true)" type="datetime-local" @change="updateDateFilter('to', $event)" /></label>
          <label><span>状态</span><select :value="auditQuery.status" @change="updateStatusFilter"><option value="">全部状态</option><option value="running">进行中</option><option value="success">成功</option><option value="partial_success">部分成功</option><option value="failed">失败</option><option value="cancelled">已取消</option></select></label>
          <label><span>类别</span><input :value="auditQuery.category" placeholder="admin / session" @change="updateTextFilter('category', $event)" /></label>
          <label><span>动作</span><input :value="auditQuery.action" placeholder="操作名称" @change="updateTextFilter('action', $event)" /></label>
          <label><span>用户 ID</span><input :value="auditQuery.user_id" placeholder="用户 ID" @change="updateTextFilter('user_id', $event)" /></label>
          <label><span>操作者</span><input :value="auditQuery.actor" placeholder="用户名" @change="updateTextFilter('actor', $event)" /></label>
          <label><span>节点 ID</span><input :value="auditQuery.instance_id" placeholder="节点 ID" @change="updateTextFilter('instance_id', $event)" /></label>
          <label><span>来源 IP</span><input :value="auditQuery.source_ip" placeholder="例如 10.0.0.1" @change="updateTextFilter('source_ip', $event)" /></label>
          <label class="audit-filter-wide"><span>请求 ID</span><input :value="auditQuery.request_id" placeholder="UUID request ID" @change="updateTextFilter('request_id', $event)" /></label>
        </div>

        <p v-if="auditError" class="notice audit-error">{{ auditError }}</p>
        <Transition name="content" mode="out-in">
          <div v-if="auditLoading && audit.items.length === 0" key="loading" class="page-empty"><LoaderCircle class="spin" :size="26" /><strong>正在加载审计记录</strong></div>
          <div v-else-if="audit.items.length === 0" key="empty" class="page-empty"><span><ClipboardList :size="24" /></span><strong>暂无匹配记录</strong><p>调整筛选条件后重试。</p></div>
          <div v-else key="audit" class="audit-table">
            <div class="audit-table-head"><span>时间 / 状态</span><span>动作与目标</span><span>节点 / 用户</span><span>请求 / 来源</span><span aria-hidden="true"></span></div>
            <article v-for="event in audit.items" :key="event.id" class="audit-row">
              <div class="audit-primary"><time>{{ formatTime(event.created_at) }}</time><span :class="['audit-status', event.status]">{{ statusLabel(event.status) }}</span><small>{{ categoryLabel(event.category) }}</small></div>
              <div class="audit-action"><strong :title="event.action || event.kind">{{ event.action || event.kind }}</strong><p :title="event.detail || event.target">{{ event.detail || event.target || '无详情' }}</p><small :title="event.target">{{ event.target || '全局操作' }}</small></div>
              <div class="audit-context"><strong :title="nodeLabel(event)">{{ nodeLabel(event) }}</strong><small v-if="event.actor">操作者 {{ event.actor }}</small><small v-if="event.user_id" :title="event.user_id">用户 {{ event.user_id }}</small><small v-if="event.instance_id" :title="event.instance_id">节点 {{ event.instance_id }}</small></div>
              <div class="audit-request"><code v-if="event.request_id" :title="event.request_id">{{ event.request_id }}</code><small v-if="event.source_ip">{{ event.source_ip }}</small><small v-if="event.operation_id" :title="event.operation_id">操作 {{ event.operation_id }}</small></div>
              <button
                class="icon-button subtle"
                type="button"
                :aria-controls="`audit-detail-${event.id}`"
                :aria-expanded="expandedAuditId === event.id"
                :aria-label="expandedAuditId === event.id ? '收起审计详情' : '展开审计详情'"
                :title="expandedAuditId === event.id ? '收起详情' : '展开详情'"
                @click="toggleAuditDetails(event)"
              ><ChevronDown :size="16" /></button>
              <div v-if="expandedAuditId === event.id" :id="`audit-detail-${event.id}`" class="audit-detail">
                <dl>
                  <div><dt>事件 ID</dt><dd>{{ event.id }}</dd></div>
                  <div><dt>请求 ID</dt><dd>{{ event.request_id || '无' }}</dd></div>
                  <div><dt>操作 ID</dt><dd>{{ event.operation_id || '无' }}</dd></div>
                  <div><dt>会话 ID</dt><dd>{{ event.session_id || '无' }}</dd></div>
                  <div><dt>用户 ID</dt><dd>{{ event.user_id || '无' }}</dd></div>
                  <div><dt>操作者</dt><dd>{{ event.actor || '无' }}</dd></div>
                  <div><dt>来源 IP</dt><dd>{{ event.source_ip || '无' }}</dd></div>
                  <div><dt>类别 / 类型</dt><dd>{{ event.category || '无' }} / {{ event.kind || '无' }}</dd></div>
                  <div><dt>创建时间</dt><dd>{{ formatTime(event.created_at) }}</dd></div>
                  <div><dt>完成时间</dt><dd>{{ event.completed_at ? formatTime(event.completed_at) : '未完成' }}</dd></div>
                  <div><dt>错误码</dt><dd>{{ event.error_code || '无' }}</dd></div>
                  <div class="audit-detail-wide"><dt>错误原因</dt><dd>{{ event.error_reason || '无' }}</dd></div>
                  <div class="audit-detail-wide"><dt>动作 / 目标</dt><dd>{{ event.action || event.kind || '无' }} / {{ event.target || '无' }}</dd></div>
                  <div class="audit-detail-wide"><dt>详情</dt><dd>{{ event.detail || '无' }}</dd></div>
                  <div class="audit-detail-wide"><dt>User-Agent</dt><dd>{{ event.user_agent || '无' }}</dd></div>
                  <div v-if="hasJsonContent(event.node_snapshot)" class="audit-detail-wide"><dt>节点快照</dt><dd><pre>{{ jsonValue(event.node_snapshot) }}</pre></dd></div>
                  <div v-if="hasJsonContent(event.metadata)" class="audit-detail-wide"><dt>元数据</dt><dd><pre>{{ jsonValue(event.metadata) }}</pre></dd></div>
                </dl>
              </div>
            </article>
          </div>
        </Transition>
        <footer class="audit-pagination">
          <span>{{ auditRange }}</span>
          <div><button class="icon-button subtle" type="button" title="上一页" :disabled="audit.page <= 1 || auditLoading" @click="$emit('auditPageChanged', audit.page - 1)"><ChevronLeft :size="16" /></button><strong>第 {{ audit.page }} / {{ Math.max(audit.pages, 1) }} 页</strong><button class="icon-button subtle" type="button" title="下一页" :disabled="audit.page >= audit.pages || auditLoading" @click="$emit('auditPageChanged', audit.page + 1)"><ChevronRight :size="16" /></button></div>
        </footer>
      </div>
    </template>
  </section>
</template>
