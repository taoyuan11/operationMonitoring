<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import {
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ClipboardList,
  Download,
  Filter,
  LoaderCircle,
  PanelRightOpen,
  RefreshCw,
  Search,
  X,
} from 'lucide-vue-next'
import type {
  AuditEvent,
  AuditExportFormat,
  AuditPage,
  AuditQuery,
} from '../types/domain'
import { formatTime } from '../utils/format'
import WorkspaceDrawer from './WorkspaceDrawer.vue'

const props = defineProps<{
  audit: AuditPage
  auditQuery: AuditQuery
  auditLoading: boolean
  auditError: string
  auditExporting: AuditExportFormat | null
}>()

const emit = defineEmits<{
  auditQueryChanged: [patch: Partial<AuditQuery>]
  auditPageChanged: [page: number]
  refreshAudit: []
  exportAudit: [format: AuditExportFormat]
}>()

type AdvancedFilterKey =
  | 'from'
  | 'to'
  | 'category'
  | 'action'
  | 'user_id'
  | 'actor'
  | 'instance_id'
  | 'source_ip'
  | 'request_id'

type AdvancedQueryPatch = Pick<AuditQuery, AdvancedFilterKey>

type AdvancedFilterDraft = {
  from: string
  to: string
  category: string
  action: string
  user_id: string
  actor: string
  instance_id: string
  source_ip: string
  request_id: string
}

type AdvancedFilterChip = {
  key: AdvancedFilterKey
  label: string
  value: string
}

const exportMenuElement = ref<HTMLElement | null>(null)
const exportMenuButton = ref<HTMLButtonElement | null>(null)
const keywordDraft = ref(props.auditQuery.keyword)
const filtersOpen = ref(false)
const exportMenuOpen = ref(false)
const selectedAuditId = ref('')
const advancedDraft = reactive<AdvancedFilterDraft>({
  from: '',
  to: '',
  category: '',
  action: '',
  user_id: '',
  actor: '',
  instance_id: '',
  source_ip: '',
  request_id: '',
})

const auditRange = computed(() => {
  if (props.audit.total === 0) return '0 条记录'
  if (props.audit.items.length === 0) return `0 / ${props.audit.total}`
  const start = (props.audit.page - 1) * props.audit.page_size + 1
  const end = Math.min(props.audit.total, start + props.audit.items.length - 1)
  return `${start}-${end} / ${props.audit.total}`
})

const selectedEvent = computed(() =>
  props.audit.items.find((event) => event.id === selectedAuditId.value) ?? null,
)

const advancedFilterChips = computed<AdvancedFilterChip[]>(() => {
  const chips: AdvancedFilterChip[] = []
  const query = props.auditQuery

  if (query.from) chips.push({ key: 'from', label: '开始', value: dateDisplay(query.from) })
  if (query.to) chips.push({ key: 'to', label: '结束', value: dateDisplay(query.to, true) })
  if (query.category) chips.push({ key: 'category', label: '类别', value: query.category })
  if (query.action) chips.push({ key: 'action', label: '动作', value: query.action })
  if (query.user_id) chips.push({ key: 'user_id', label: '用户', value: query.user_id })
  if (query.actor) chips.push({ key: 'actor', label: '操作者', value: query.actor })
  if (query.instance_id) chips.push({ key: 'instance_id', label: '节点', value: query.instance_id })
  if (query.source_ip) chips.push({ key: 'source_ip', label: '来源 IP', value: query.source_ip })
  if (query.request_id) chips.push({ key: 'request_id', label: '请求 ID', value: query.request_id })

  return chips
})

watch(() => props.auditQuery.keyword, (keyword) => {
  keywordDraft.value = keyword
})

watch(() => props.auditExporting, (format) => {
  if (format) exportMenuOpen.value = false
})

watch(
  () => props.audit.items.map((event) => event.id).join('\u0000'),
  () => {
    if (selectedAuditId.value && !selectedEvent.value) selectedAuditId.value = ''
  },
)

onMounted(() => {
  document.addEventListener('pointerdown', handleDocumentPointerdown)
  document.addEventListener('keydown', handleDocumentKeydown)
})

onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', handleDocumentPointerdown)
  document.removeEventListener('keydown', handleDocumentKeydown)
})

function dateInputValue(timestamp: number | null, endOfMinute = false) {
  if (!timestamp) return ''
  const date = new Date((timestamp - (endOfMinute ? 60 : 0)) * 1000)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}T${pad(date.getHours())}:${pad(date.getMinutes())}`
}

function dateDisplay(timestamp: number, endOfMinute = false) {
  return dateInputValue(timestamp, endOfMinute).replace('T', ' ')
}

function timestampFromInput(value: string, endOfMinute = false) {
  if (!value) return null
  const timestamp = Date.parse(value)
  return Number.isNaN(timestamp) ? null : Math.floor(timestamp / 1000) + (endOfMinute ? 60 : 0)
}

function statusLabel(status: AuditEvent['status']) {
  return {
    running: '进行中',
    success: '成功',
    partial_success: '部分成功',
    failed: '失败',
    cancelled: '已取消',
  }[status]
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
  if (value === null || value === undefined) return '无'
  return JSON.stringify(value, null, 2)
}

function eventTitle(event: AuditEvent) {
  return event.action || event.kind || '未命名事件'
}

function applyKeyword() {
  if (keywordDraft.value === props.auditQuery.keyword) return
  emit('auditQueryChanged', { keyword: keywordDraft.value })
}

function clearKeyword() {
  keywordDraft.value = ''
  applyKeyword()
}

function updateStatus(event: Event) {
  emit('auditQueryChanged', {
    status: (event.target as HTMLSelectElement).value as AuditQuery['status'],
  })
}

function syncAdvancedDraft() {
  Object.assign(advancedDraft, {
    from: dateInputValue(props.auditQuery.from),
    to: dateInputValue(props.auditQuery.to, true),
    category: props.auditQuery.category,
    action: props.auditQuery.action,
    user_id: props.auditQuery.user_id,
    actor: props.auditQuery.actor,
    instance_id: props.auditQuery.instance_id,
    source_ip: props.auditQuery.source_ip,
    request_id: props.auditQuery.request_id,
  })
}

function openAdvancedFilters() {
  selectedAuditId.value = ''
  exportMenuOpen.value = false
  syncAdvancedDraft()
  filtersOpen.value = true
}

function closeAdvancedFilters() {
  filtersOpen.value = false
}

function advancedPatch(): AdvancedQueryPatch {
  return {
    from: timestampFromInput(advancedDraft.from),
    to: timestampFromInput(advancedDraft.to, true),
    category: advancedDraft.category.trim(),
    action: advancedDraft.action.trim(),
    user_id: advancedDraft.user_id.trim(),
    actor: advancedDraft.actor.trim(),
    instance_id: advancedDraft.instance_id.trim(),
    source_ip: advancedDraft.source_ip.trim(),
    request_id: advancedDraft.request_id.trim(),
  }
}

function advancedFiltersChanged(patch: AdvancedQueryPatch) {
  return patch.from !== props.auditQuery.from
    || patch.to !== props.auditQuery.to
    || patch.category !== props.auditQuery.category
    || patch.action !== props.auditQuery.action
    || patch.user_id !== props.auditQuery.user_id
    || patch.actor !== props.auditQuery.actor
    || patch.instance_id !== props.auditQuery.instance_id
    || patch.source_ip !== props.auditQuery.source_ip
    || patch.request_id !== props.auditQuery.request_id
}

function applyAdvancedFilters() {
  const patch = advancedPatch()
  filtersOpen.value = false
  if (advancedFiltersChanged(patch)) emit('auditQueryChanged', patch)
}

function removeAdvancedFilter(key: AdvancedFilterKey) {
  const patch = (key === 'from' || key === 'to')
    ? { [key]: null }
    : { [key]: '' }
  emit('auditQueryChanged', patch as Partial<AuditQuery>)
}

function clearAdvancedFilters() {
  emit('auditQueryChanged', {
    from: null,
    to: null,
    category: '',
    action: '',
    user_id: '',
    actor: '',
    instance_id: '',
    source_ip: '',
    request_id: '',
  })
}

function selectEvent(event: AuditEvent) {
  filtersOpen.value = false
  exportMenuOpen.value = false
  selectedAuditId.value = event.id
}

function closeEventDetails() {
  selectedAuditId.value = ''
}

function exportAudit(format: AuditExportFormat) {
  closeExportMenu()
  emit('exportAudit', format)
}

async function openExportMenu(itemIndex = 0) {
  if (props.auditExporting) return
  exportMenuOpen.value = true
  await nextTick()
  const items = exportMenuItems()
  const targetIndex = itemIndex < 0 ? items.length - 1 : itemIndex
  items[targetIndex]?.focus()
}

function toggleExportMenu() {
  if (exportMenuOpen.value) {
    closeExportMenu()
    return
  }
  void openExportMenu()
}

function handleExportMenuButtonKeydown(event: KeyboardEvent) {
  if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return
  event.preventDefault()
  void openExportMenu(event.key === 'ArrowUp' ? -1 : 0)
}

function exportMenuItems() {
  return Array.from(
    exportMenuElement.value?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]') || [],
  )
}

function closeExportMenu(restoreFocus = false) {
  if (!exportMenuOpen.value) return
  exportMenuOpen.value = false
  if (restoreFocus) void nextTick(() => exportMenuButton.value?.focus())
}

function handleExportMenuKeydown(event: KeyboardEvent) {
  if (event.key === 'Escape') {
    event.preventDefault()
    closeExportMenu(true)
    return
  }
  const items = exportMenuItems()
  if (!items.length) return
  const currentIndex = items.indexOf(document.activeElement as HTMLButtonElement)
  let nextIndex: number | null = null
  if (event.key === 'ArrowDown') nextIndex = currentIndex < 0 ? 0 : (currentIndex + 1) % items.length
  if (event.key === 'ArrowUp') nextIndex = currentIndex < 0 ? items.length - 1 : (currentIndex - 1 + items.length) % items.length
  if (event.key === 'Home') nextIndex = 0
  if (event.key === 'End') nextIndex = items.length - 1
  if (nextIndex === null) return
  event.preventDefault()
  items[nextIndex]?.focus()
}

function handleExportMenuFocusout(event: FocusEvent) {
  if (event.relatedTarget instanceof Node && exportMenuElement.value?.contains(event.relatedTarget)) return
  closeExportMenu()
}

function handleDocumentPointerdown(event: PointerEvent) {
  if (!exportMenuOpen.value || !(event.target instanceof Node)) return
  if (!exportMenuElement.value?.contains(event.target)) closeExportMenu()
}

function handleDocumentKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape' || !exportMenuOpen.value) return
  event.preventDefault()
  closeExportMenu(true)
}
</script>

<template>
  <section class="management-page audit-workspace">
    <header class="audit-workspace-header">
      <div class="audit-workspace-heading-icon"><ClipboardList :size="21" /></div>
      <div class="audit-workspace-heading">
        <span class="section-kicker">Audit trail</span>
        <h2>统一审计</h2>
        <p>管理员操作、安全事件与远程会话记录</p>
      </div>
      <span class="page-count">{{ auditRange }}</span>
    </header>

    <div class="audit-workspace-panel" :aria-busy="auditLoading">
      <div class="audit-workspace-toolbar">
        <form class="audit-workspace-search" role="search" @submit.prevent="applyKeyword">
          <Search :size="15" aria-hidden="true" />
          <label class="audit-workspace-sr-only" for="audit-keyword">搜索审计记录</label>
          <input
            id="audit-keyword"
            v-model="keywordDraft"
            type="search"
            placeholder="搜索动作、目标或详情"
            @change="applyKeyword"
          />
          <button
            v-if="keywordDraft"
            class="audit-workspace-input-clear"
            type="button"
            title="清除关键字"
            aria-label="清除关键字"
            @click="clearKeyword"
          ><X :size="14" /></button>
        </form>

        <label class="audit-workspace-status-filter">
          <span class="audit-workspace-sr-only">状态</span>
          <select :value="auditQuery.status" title="按状态筛选" @change="updateStatus">
            <option value="">全部状态</option>
            <option value="running">进行中</option>
            <option value="success">成功</option>
            <option value="partial_success">部分成功</option>
            <option value="failed">失败</option>
            <option value="cancelled">已取消</option>
          </select>
        </label>

        <div class="audit-workspace-toolbar-actions">
          <button
            class="text-button audit-workspace-filter-button"
            type="button"
            :aria-label="advancedFilterChips.length ? `高级筛选，已应用 ${advancedFilterChips.length} 项` : '高级筛选'"
            @click="openAdvancedFilters"
          >
            <Filter :size="15" />
            <span>筛选</span>
            <em v-if="advancedFilterChips.length">{{ advancedFilterChips.length }}</em>
          </button>
          <button
            class="icon-button subtle"
            type="button"
            title="刷新审计记录"
            aria-label="刷新审计记录"
            :disabled="auditLoading"
            @click="$emit('refreshAudit')"
          ><RefreshCw :class="{ spin: auditLoading }" :size="16" /></button>
          <div ref="exportMenuElement" class="audit-workspace-export" @focusout="handleExportMenuFocusout">
            <button
              ref="exportMenuButton"
              class="text-button audit-workspace-export-trigger"
              type="button"
              :disabled="Boolean(auditExporting)"
              aria-haspopup="menu"
              :aria-expanded="exportMenuOpen"
              @click="toggleExportMenu"
              @keydown="handleExportMenuButtonKeydown"
            >
              <LoaderCircle v-if="auditExporting" class="spin" :size="15" />
              <Download v-else :size="15" />
              <span>导出</span>
              <ChevronDown :size="13" />
            </button>
            <div v-if="exportMenuOpen" class="audit-workspace-export-menu" role="menu" @keydown="handleExportMenuKeydown">
              <button type="button" role="menuitem" @click="exportAudit('csv')">
                <span>CSV 表格</span><small>.csv</small>
              </button>
              <button type="button" role="menuitem" @click="exportAudit('json')">
                <span>JSON 数据</span><small>.json</small>
              </button>
            </div>
          </div>
        </div>
      </div>

      <div v-if="advancedFilterChips.length" class="audit-workspace-chips" aria-label="已应用的高级筛选">
        <div class="audit-workspace-chip-list">
          <button
            v-for="chip in advancedFilterChips"
            :key="chip.key"
            type="button"
            :title="`移除${chip.label}筛选`"
            @click="removeAdvancedFilter(chip.key)"
          >
            <span>{{ chip.label }}</span>
            <strong>{{ chip.value }}</strong>
            <X :size="12" />
          </button>
        </div>
        <button class="audit-workspace-clear-filters" type="button" @click="clearAdvancedFilters">清除全部</button>
      </div>

      <p v-if="auditError" class="audit-workspace-error" role="alert">{{ auditError }}</p>
      <div v-if="auditLoading && audit.items.length" class="audit-workspace-loading-line" role="status">
        <span class="audit-workspace-sr-only">正在刷新审计记录</span>
      </div>

      <div class="audit-workspace-records">
        <div v-if="auditLoading && audit.items.length === 0" class="audit-workspace-empty" role="status">
          <LoaderCircle class="spin" :size="25" />
          <strong>正在加载审计记录</strong>
        </div>
        <div v-else-if="audit.items.length === 0" class="audit-workspace-empty">
          <span><ClipboardList :size="22" /></span>
          <strong>暂无匹配记录</strong>
          <p>调整筛选条件后重试。</p>
        </div>
        <div v-else class="audit-workspace-table" role="table" aria-label="审计记录">
          <div class="audit-workspace-table-head" role="row">
            <span role="columnheader">时间 / 状态</span>
            <span role="columnheader">动作与目标</span>
            <span role="columnheader">节点 / 用户</span>
            <span role="columnheader">请求 / 来源</span>
            <span role="columnheader" aria-label="查看详情"></span>
          </div>
          <div class="audit-workspace-table-body" role="rowgroup">
            <article
              v-for="event in audit.items"
              :key="event.id"
              class="audit-workspace-row"
              :class="{ selected: selectedAuditId === event.id }"
              role="row"
              tabindex="0"
              :aria-label="`查看审计事件：${eventTitle(event)}`"
              :aria-selected="selectedAuditId === event.id"
              aria-haspopup="dialog"
              :aria-expanded="selectedAuditId === event.id"
              @click="selectEvent(event)"
              @keydown.enter="selectEvent(event)"
              @keydown.space.prevent="selectEvent(event)"
            >
              <div class="audit-workspace-primary" role="cell">
                <time>{{ formatTime(event.created_at) }}</time>
                <div>
                  <span :class="['audit-workspace-status', event.status]">{{ statusLabel(event.status) }}</span>
                  <small>{{ categoryLabel(event.category) }}</small>
                </div>
              </div>
              <div class="audit-workspace-action" role="cell">
                <strong :title="eventTitle(event)">{{ eventTitle(event) }}</strong>
                <p :title="event.detail || event.target">{{ event.detail || event.target || '无详情' }}</p>
                <small :title="event.target">{{ event.target || '全局操作' }}</small>
              </div>
              <div class="audit-workspace-context" role="cell">
                <strong :title="nodeLabel(event)">{{ nodeLabel(event) }}</strong>
                <small v-if="event.actor">操作者 {{ event.actor }}</small>
                <small v-if="event.user_id" :title="event.user_id">用户 {{ event.user_id }}</small>
              </div>
              <div class="audit-workspace-request" role="cell">
                <code v-if="event.request_id" :title="event.request_id">{{ event.request_id }}</code>
                <small v-if="event.source_ip">{{ event.source_ip }}</small>
                <small v-if="event.operation_id" :title="event.operation_id">操作 {{ event.operation_id }}</small>
              </div>
              <PanelRightOpen class="audit-workspace-row-arrow" :size="16" role="cell" aria-hidden="true" />
            </article>
          </div>
        </div>
      </div>

      <footer class="audit-workspace-pagination">
        <span>{{ auditRange }}</span>
        <div>
          <button
            class="icon-button subtle"
            type="button"
            title="上一页"
            aria-label="上一页"
            :disabled="audit.page <= 1 || auditLoading"
            @click="$emit('auditPageChanged', audit.page - 1)"
          ><ChevronLeft :size="16" /></button>
          <strong>第 {{ audit.page }} / {{ Math.max(audit.pages, 1) }} 页</strong>
          <button
            class="icon-button subtle"
            type="button"
            title="下一页"
            aria-label="下一页"
            :disabled="audit.page >= audit.pages || auditLoading"
            @click="$emit('auditPageChanged', audit.page + 1)"
          ><ChevronRight :size="16" /></button>
        </div>
      </footer>
    </div>

    <WorkspaceDrawer
      v-if="filtersOpen"
      title="高级筛选"
      description="组合条件后一次应用，不会在输入过程中刷新列表。"
      size="medium"
      :modal="true"
      @close="closeAdvancedFilters"
    >
      <form class="audit-workspace-filter-form" @submit.prevent="applyAdvancedFilters">
        <div class="audit-workspace-filter-grid">
          <label><span>开始时间</span><input v-model="advancedDraft.from" type="datetime-local" /></label>
          <label><span>结束时间</span><input v-model="advancedDraft.to" type="datetime-local" /></label>
          <label><span>类别</span><input v-model="advancedDraft.category" placeholder="admin / session" /></label>
          <label><span>动作</span><input v-model="advancedDraft.action" placeholder="操作名称" /></label>
          <label><span>用户 ID</span><input v-model="advancedDraft.user_id" placeholder="用户 ID" /></label>
          <label><span>操作者</span><input v-model="advancedDraft.actor" placeholder="用户名" /></label>
          <label><span>节点 ID</span><input v-model="advancedDraft.instance_id" placeholder="节点 ID" /></label>
          <label><span>来源 IP</span><input v-model="advancedDraft.source_ip" placeholder="例如 10.0.0.1" /></label>
          <label class="audit-workspace-filter-wide"><span>请求 ID</span><input v-model="advancedDraft.request_id" placeholder="UUID request ID" /></label>
        </div>
      </form>
      <template #footer>
        <button class="text-button" type="button" @click="closeAdvancedFilters">取消</button>
        <button class="primary-button" type="button" @click="applyAdvancedFilters">应用筛选</button>
      </template>
    </WorkspaceDrawer>

    <WorkspaceDrawer
      v-if="selectedEvent"
      :title="eventTitle(selectedEvent)"
      :description="`${formatTime(selectedEvent.created_at)} · ${categoryLabel(selectedEvent.category)} · ${statusLabel(selectedEvent.status)}`"
      size="wide"
      :modal="false"
      @close="closeEventDetails"
    >
      <div class="audit-workspace-detail-summary">
        <span :class="['audit-workspace-status', selectedEvent.status]">{{ statusLabel(selectedEvent.status) }}</span>
        <strong>{{ selectedEvent.target || '全局操作' }}</strong>
        <p>{{ selectedEvent.detail || '无详情' }}</p>
      </div>

      <dl class="audit-workspace-detail-list">
        <div><dt>事件 ID</dt><dd>{{ selectedEvent.id }}</dd></div>
        <div><dt>请求 ID</dt><dd>{{ selectedEvent.request_id || '无' }}</dd></div>
        <div><dt>操作 ID</dt><dd>{{ selectedEvent.operation_id || '无' }}</dd></div>
        <div><dt>会话 ID</dt><dd>{{ selectedEvent.session_id || '无' }}</dd></div>
        <div><dt>用户 ID</dt><dd>{{ selectedEvent.user_id || '无' }}</dd></div>
        <div><dt>节点 ID</dt><dd>{{ selectedEvent.instance_id || '无' }}</dd></div>
        <div><dt>操作者</dt><dd>{{ selectedEvent.actor || '无' }}</dd></div>
        <div><dt>来源 IP</dt><dd>{{ selectedEvent.source_ip || '无' }}</dd></div>
        <div><dt>类别</dt><dd>{{ selectedEvent.category || '无' }}</dd></div>
        <div><dt>事件类型</dt><dd>{{ selectedEvent.kind || '无' }}</dd></div>
        <div><dt>创建时间</dt><dd>{{ formatTime(selectedEvent.created_at) }}</dd></div>
        <div><dt>完成时间</dt><dd>{{ selectedEvent.completed_at ? formatTime(selectedEvent.completed_at) : '未完成' }}</dd></div>
        <div><dt>错误码</dt><dd>{{ selectedEvent.error_code || '无' }}</dd></div>
        <div class="audit-workspace-detail-wide"><dt>错误原因</dt><dd>{{ selectedEvent.error_reason || '无' }}</dd></div>
        <div class="audit-workspace-detail-wide"><dt>User-Agent</dt><dd>{{ selectedEvent.user_agent || '无' }}</dd></div>
        <div class="audit-workspace-detail-wide"><dt>节点快照</dt><dd><pre>{{ jsonValue(selectedEvent.node_snapshot) }}</pre></dd></div>
        <div class="audit-workspace-detail-wide"><dt>元数据</dt><dd><pre>{{ jsonValue(selectedEvent.metadata) }}</pre></dd></div>
      </dl>
    </WorkspaceDrawer>
  </section>
</template>
