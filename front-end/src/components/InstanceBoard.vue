<script setup lang="ts">
import { computed, ref } from 'vue'
import {
  ArrowDownWideNarrow,
  Box,
  Cpu,
  Grid3X3,
  HardDrive,
  ListFilter,
  MemoryStick,
  MoreHorizontal,
  Pause,
  Pencil,
  Play,
  RotateCcw,
  Rows3,
  Search,
  Server,
  Terminal,
  Trash2,
  Wifi,
  WifiOff,
  X,
  Zap,
} from 'lucide-vue-next'
import CountryFlag from './CountryFlag.vue'
import OperatingSystemLogo from './OperatingSystemLogo.vue'
import { getCountryOption } from '../data/countries'
import type { CommandRecord, Instance, ViewMode } from '../types/domain'
import { formatBytes, formatDuration, formatPercent, formatTime, metricPercent } from '../utils/format'

type StatusFilter = 'all' | 'online' | 'offline'
type SortMode = 'default' | 'cpu' | 'memory' | 'name'

const props = defineProps<{
  instances: Instance[]
  commands: CommandRecord[]
  isAdmin: boolean
  viewMode: ViewMode
}>()

const emit = defineEmits<{
  'update:viewMode': [value: ViewMode]
  edit: [instance: Instance]
  terminal: [instance: Instance]
  disable: [instance: Instance]
  delete: [instance: Instance]
  runCommand: [instance: Instance, command: CommandRecord]
}>()

const searchQuery = ref('')
const statusFilter = ref<StatusFilter>('all')
const sortMode = ref<SortMode>('default')
const openMenuId = ref<string | null>(null)
const nameCollator = new Intl.Collator('zh-CN', { numeric: true, sensitivity: 'base' })

const filteredInstances = computed(() => {
  const query = searchQuery.value.trim().toLocaleLowerCase()
  const items = props.instances.filter((instance) => {
    const matchesStatus = statusFilter.value === 'all'
      || (statusFilter.value === 'online' ? instance.online : !instance.online)
    if (!matchesStatus || !query) return matchesStatus

    return [instance.name, instance.hostname, instanceCountry(instance), instance.remark]
      .some((value) => value?.toLocaleLowerCase().includes(query))
  })

  if (sortMode.value === 'default') return items

  return [...items].sort((left, right) => {
    if (sortMode.value === 'name') {
      return nameCollator.compare(instanceName(left), instanceName(right))
    }

    const leftValue = sortMode.value === 'cpu' ? cpuPercent(left) : memoryPercent(left)
    const rightValue = sortMode.value === 'cpu' ? cpuPercent(right) : memoryPercent(right)
    if (leftValue === null && rightValue === null) return 0
    if (leftValue === null) return 1
    if (rightValue === null) return -1
    return rightValue - leftValue
  })
})

function instanceName(instance: Instance) {
  return instance.name || instance.hostname || '未命名节点'
}

function instanceCountry(instance: Instance) {
  return getCountryOption(instance.country_code)?.name
    || instance.country
    || '未设置国家'
}

function isKnownMetric(value: number | null | undefined): value is number {
  return typeof value === 'number' && Number.isFinite(value)
}

function cpuPercent(instance: Instance) {
  return isKnownMetric(instance.metrics?.cpu_percent) ? instance.metrics.cpu_percent : null
}

function memoryPercent(instance: Instance) {
  if (!instance.metrics || instance.metrics.memory_total <= 0) return null
  return metricPercent(instance.metrics.memory_used, instance.metrics.memory_total)
}

function diskPercent(instance: Instance) {
  if (!instance.metrics || instance.metrics.disk_total <= 0) return null
  return metricPercent(instance.metrics.disk_used, instance.metrics.disk_total)
}

function metricWidth(value: number | null | undefined) {
  if (!isKnownMetric(value)) return 0
  return Math.max(0, Math.min(100, value))
}

function metricLevel(value: number | null | undefined) {
  if (!isKnownMetric(value)) return ''
  if (value >= 85) return 'high'
  if (value >= 65) return 'elevated'
  return ''
}

function resetFilters() {
  searchQuery.value = ''
  statusFilter.value = 'all'
  sortMode.value = 'default'
  openMenuId.value = null
}

function toggleMenu(instanceId: string) {
  openMenuId.value = openMenuId.value === instanceId ? null : instanceId
}

function disableInstance(instance: Instance) {
  openMenuId.value = null
  emit('disable', instance)
}

function deleteInstance(instance: Instance) {
  openMenuId.value = null
  emit('delete', instance)
}
</script>

<template>
  <section class="content-pane" @click="openMenuId = null" @keydown.esc="openMenuId = null">
    <div class="section-head">
      <div>
        <span class="section-kicker">Live nodes</span>
        <h2>运行节点 <em>{{ instances.length }}</em></h2>
        <p>节点状态与资源指标每 5 秒自动刷新</p>
      </div>
      <div class="segmented" aria-label="切换视图">
        <button
          :class="{ active: viewMode === 'grid' }"
          type="button"
          title="卡片视图"
          aria-label="切换到卡片视图"
          :aria-pressed="viewMode === 'grid'"
          @click="$emit('update:viewMode', 'grid')"
        >
          <Grid3X3 :size="16" />
        </button>
        <button
          :class="{ active: viewMode === 'rows' }"
          type="button"
          title="列表视图"
          aria-label="切换到列表视图"
          :aria-pressed="viewMode === 'rows'"
          @click="$emit('update:viewMode', 'rows')"
        >
          <Rows3 :size="16" />
        </button>
      </div>
    </div>

    <Transition name="content">
      <div v-if="instances.length" class="board-toolbar">
        <div class="instance-search" role="search">
          <Search :size="16" aria-hidden="true" />
          <input
            v-model="searchQuery"
            type="search"
            aria-label="搜索节点名称、主机名、地区或备注"
            placeholder="搜索名称、主机名、地区或备注"
          />
          <Transition name="fade-scale">
            <button
              v-if="searchQuery"
              class="clear-search"
              type="button"
              title="清除搜索"
              aria-label="清除节点搜索"
              @click="searchQuery = ''"
            >
              <X :size="14" />
            </button>
          </Transition>
        </div>

        <div class="toolbar-group status-filter">
          <span class="toolbar-label"><ListFilter :size="14" aria-hidden="true" />状态</span>
          <div class="segmented filter-segmented" aria-label="按节点状态筛选">
            <button
              :class="{ active: statusFilter === 'all' }"
              type="button"
              title="显示全部节点"
              aria-label="显示全部节点"
              :aria-pressed="statusFilter === 'all'"
              @click="statusFilter = 'all'"
            >
              全部
            </button>
            <button
              :class="{ active: statusFilter === 'online' }"
              type="button"
              title="只显示在线节点"
              aria-label="只显示在线节点"
              :aria-pressed="statusFilter === 'online'"
              @click="statusFilter = 'online'"
            >
              <Wifi :size="13" aria-hidden="true" />在线
            </button>
            <button
              :class="{ active: statusFilter === 'offline' }"
              type="button"
              title="只显示离线节点"
              aria-label="只显示离线节点"
              :aria-pressed="statusFilter === 'offline'"
              @click="statusFilter = 'offline'"
            >
              <WifiOff :size="13" aria-hidden="true" />离线
            </button>
          </div>
        </div>

        <label class="sort-control">
          <span><ArrowDownWideNarrow :size="14" aria-hidden="true" />排序</span>
          <select v-model="sortMode" aria-label="节点排序方式">
            <option value="default">默认顺序</option>
            <option value="cpu">CPU 占用</option>
            <option value="memory">内存占用</option>
            <option value="name">节点名称</option>
          </select>
        </label>

        <output class="result-count" aria-live="polite">
          显示 {{ filteredInstances.length }} / {{ instances.length }} 个节点
        </output>
      </div>
    </Transition>

    <Transition name="content" mode="out-in">
      <div v-if="instances.length === 0" key="empty" class="empty-state">
        <span class="empty-icon"><Server :size="28" /></span>
        <strong>暂无已接入节点</strong>
        <span>{{ isAdmin ? '请前往“接入审核”页面处理新节点申请。' : '节点通过管理员审核并完成接入后会显示在这里。' }}</span>
      </div>

      <div v-else-if="filteredInstances.length === 0" key="filtered-empty" class="empty-state filtered-empty">
        <span class="empty-icon"><Search :size="28" /></span>
        <strong>没有符合条件的节点</strong>
        <span>尝试更换关键词或状态筛选条件。</span>
        <button
          class="text-button"
          type="button"
          title="重置筛选条件"
          aria-label="重置节点筛选条件"
          @click="resetFilters"
        >
          <RotateCcw :size="14" />重置筛选
        </button>
      </div>

      <TransitionGroup v-else key="instances" name="instance" tag="div" :class="['instance-list', viewMode]">
      <article
        v-for="instance in filteredInstances"
        :key="instance.id"
        :class="['instance-card', { offline: !instance.online }]"
      >
        <header class="instance-main">
          <div class="instance-identity">
            <OperatingSystemLogo class="server-icon" :os="instance.os" />
            <div>
              <div class="title-line">
                <h3 :title="instanceName(instance)">{{ instanceName(instance) }}</h3>
                <span :class="['status-badge', { online: instance.online }]">
                  <i></i>{{ instance.online ? '在线' : '离线' }}
                </span>
              </div>
              <p class="instance-location">
                <span>
                  <CountryFlag :code="instance.country_code" :name="instanceCountry(instance)" />
                  {{ instanceCountry(instance) }}
                </span>
                <span>{{ instance.os }}/{{ instance.arch }}</span>
              </p>
            </div>
          </div>
          <div v-if="isAdmin" class="instance-actions">
            <button
              class="icon-button subtle"
              type="button"
              title="编辑节点"
              :aria-label="`编辑节点 ${instanceName(instance)}`"
              @click="$emit('edit', instance)"
            >
              <Pencil :size="15" />
            </button>
            <button
              class="icon-button subtle"
              type="button"
              title="打开 Web 终端"
              :aria-label="`打开节点 ${instanceName(instance)} 的 Web 终端`"
              @click="$emit('terminal', instance)"
            >
              <Terminal :size="15" />
            </button>
            <div class="more-actions">
              <button
                class="icon-button subtle"
                type="button"
                title="更多操作"
                :aria-label="`打开节点 ${instanceName(instance)} 的更多操作`"
                aria-haspopup="menu"
                :aria-expanded="openMenuId === instance.id"
                @click.stop="toggleMenu(instance.id)"
              >
                <MoreHorizontal :size="16" />
              </button>
              <Transition name="menu">
                <div
                  v-if="openMenuId === instance.id"
                  class="more-menu"
                  role="menu"
                  style="display: flex; flex-direction: column"
                  @click.stop
                >
                  <button
                    type="button"
                    role="menuitem"
                    title="停用节点"
                    :aria-label="`停用节点 ${instanceName(instance)}`"
                    @click="disableInstance(instance)"
                  >
                    <Pause :size="14" />停用节点
                  </button>
                  <button
                    class="danger"
                    type="button"
                    role="menuitem"
                    title="删除节点"
                    :aria-label="`删除节点 ${instanceName(instance)}`"
                    @click="deleteInstance(instance)"
                  >
                    <Trash2 :size="14" />删除节点
                  </button>
                </div>
              </Transition>
            </div>
          </div>
        </header>

        <p v-if="instance.remark" class="instance-remark">{{ instance.remark }}</p>

        <div class="metrics">
          <div :class="['metric', metricLevel(instance.metrics?.cpu_percent)]">
            <div class="metric-label"><span><Cpu :size="15" />CPU</span><strong>{{ formatPercent(instance.metrics?.cpu_percent) }}</strong></div>
            <i v-if="isKnownMetric(instance.metrics?.cpu_percent)"><b :style="{ width: `${metricWidth(instance.metrics?.cpu_percent)}%` }"></b></i>
            <small v-else class="metric-unavailable">未检测到可用数据</small>
          </div>
          <div :class="['metric', metricLevel(memoryPercent(instance))]">
            <div class="metric-label"><span><MemoryStick :size="15" />内存</span><strong>{{ formatPercent(memoryPercent(instance)) }}</strong></div>
            <i v-if="isKnownMetric(memoryPercent(instance))"><b :style="{ width: `${metricWidth(memoryPercent(instance))}%` }"></b></i>
            <small v-if="isKnownMetric(memoryPercent(instance))">{{ formatBytes(instance.metrics?.memory_used) }} / {{ formatBytes(instance.metrics?.memory_total) }}</small>
            <small v-else class="metric-unavailable">未检测到可用数据</small>
          </div>
          <div :class="['metric', metricLevel(diskPercent(instance))]">
            <div class="metric-label"><span><HardDrive :size="15" />磁盘</span><strong>{{ formatPercent(diskPercent(instance)) }}</strong></div>
            <i v-if="isKnownMetric(diskPercent(instance))"><b :style="{ width: `${metricWidth(diskPercent(instance))}%` }"></b></i>
            <small v-if="isKnownMetric(diskPercent(instance))">{{ formatBytes(instance.metrics?.disk_used) }} / {{ formatBytes(instance.metrics?.disk_total) }}</small>
            <small v-else class="metric-unavailable">未检测到可用数据</small>
          </div>
          <div :class="['metric', metricLevel(instance.metrics?.gpu_percent)]">
            <div class="metric-label"><span><Zap :size="15" />GPU</span><strong>{{ formatPercent(instance.metrics?.gpu_percent) }}</strong></div>
            <i v-if="isKnownMetric(instance.metrics?.gpu_percent)"><b :style="{ width: `${metricWidth(instance.metrics?.gpu_percent)}%` }"></b></i>
            <small v-else class="metric-unavailable">未检测到可用数据</small>
          </div>
        </div>

        <div class="meta-grid">
          <span><Box :size="14" /><i>运行时长</i><strong>{{ formatDuration(instance.metrics?.uptime_seconds) }}</strong></span>
          <span><i>网络接收</i><strong>{{ formatBytes(instance.metrics?.network_rx) }}</strong></span>
          <span><i>网络发送</i><strong>{{ formatBytes(instance.metrics?.network_tx) }}</strong></span>
          <span><i>最后上报</i><strong>{{ formatTime(instance.last_seen) }}</strong></span>
        </div>

        <div v-if="isAdmin && commands.length" class="quick-row">
          <span>快捷操作</span>
          <button
            v-for="command in commands"
            :key="command.id"
            class="command-chip"
            type="button"
            :title="`运行命令：${command.name}`"
            :aria-label="`在节点 ${instanceName(instance)} 上运行命令 ${command.name}`"
            @click="$emit('runCommand', instance, command)"
          >
            <Play :size="13" />{{ command.name }}
          </button>
        </div>
      </article>
      </TransitionGroup>
    </Transition>
  </section>
</template>
