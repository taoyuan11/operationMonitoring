<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import {
  Activity,
  Box,
  Clock3,
  Cpu,
  FileCog,
  Gauge,
  HardDrive,
  Info,
  LoaderCircle,
  MapPin,
  MemoryStick,
  Monitor,
  Network,
  Pause,
  Pencil,
  Play,
  Radio,
  RefreshCw,
  Server,
  ShieldAlert,
  Terminal,
  Timer,
  Trash2,
  UploadCloud,
  Wifi,
  WifiOff,
  X,
  Zap,
} from 'lucide-vue-next'
import CountryFlag from './CountryFlag.vue'
import DockerManagerPanel from './DockerManagerPanel.vue'
import FileManagerPanel from './FileManagerPanel.vue'
import MetricHistoryChart from './MetricHistoryChart.vue'
import OperatingSystemLogo from './OperatingSystemLogo.vue'
import { api } from '../api/http'
import { getDockerStatus } from '../api/docker'
import { getCountryOption } from '../data/countries'
import type {
  AdminDeviceProfileResponse,
  CommandRecord,
  Instance,
  Metric,
  PublicDeviceProfileResponse,
} from '../types/domain'
import type { DockerStatus } from '../types/docker'
import { formatBytes, formatDuration, formatTime, metricPercent } from '../utils/format'

type DetailTab = 'details' | 'actions' | 'files' | 'docker'
type HistoryRange = 'day' | 'week' | 'month'

const historyRanges: Array<{
  value: HistoryRange
  label: string
  seconds: number
  bucketSeconds: number
}> = [
  { value: 'day', label: '日', seconds: 24 * 3600, bucketSeconds: 5 * 60 },
  { value: 'week', label: '周', seconds: 7 * 24 * 3600, bucketSeconds: 30 * 60 },
  { value: 'month', label: '月', seconds: 30 * 24 * 3600, bucketSeconds: 2 * 3600 },
]

const props = defineProps<{
  instance: Instance
  isAdmin: boolean
  commands: CommandRecord[]
  loading: boolean
}>()

const emit = defineEmits<{
  close: []
  edit: [instance: Instance]
  terminal: [instance: Instance]
  remoteDesktop: [instance: Instance]
  disable: [instance: Instance]
  delete: [instance: Instance]
  runCommand: [instance: Instance, command: CommandRecord]
}>()

const activeTab = ref<DetailTab>('details')
const historyRange = ref<HistoryRange>('day')
const historyMetrics = ref<Metric[]>([])
const historyLoading = ref(false)
const historyError = ref('')
const historyFrom = ref(0)
const historyTo = ref(0)
const dockerStatus = ref<DockerStatus | null>(null)
const publicDeviceProfile = ref<PublicDeviceProfileResponse | null>(null)
const adminDeviceProfile = ref<AdminDeviceProfileResponse | null>(null)
const deviceProfileLoading = ref(false)
const deviceProfileError = ref('')
const adminDeviceProfileError = ref('')
let dockerStatusRequest = 0
let deviceProfileRequest = 0
let dockerStatusTimer: ReturnType<typeof setInterval> | null = null
let historyAbort: AbortController | null = null

watch(
  () => props.instance.id,
  () => {
    activeTab.value = 'details'
    dockerStatus.value = null
    void loadDeviceProfile()
    if (props.isAdmin) {
      void loadDockerStatus()
      startDockerStatusPolling()
    }
  },
  { immediate: true },
)

const supportsFiles = computed(() =>
  props.instance.capabilities?.includes('file_manager_v1') === true,
)

const supportsRemoteDesktop = computed(() =>
  props.instance.os.trim().toLowerCase().includes('windows')
    && props.instance.capabilities?.includes('remote_desktop_v1') === true,
)

const supportsDocker = computed(() => dockerStatus.value?.protocol_supported === true)

watch(
  () => props.instance.online,
  (online, previousOnline) => {
    if (props.isAdmin) void loadDockerStatus()
    if (online && previousOnline === false && !publicDeviceProfile.value?.profile) {
      void loadDeviceProfile()
    }
  },
)

watch(
  [() => props.isAdmin, supportsFiles, supportsDocker],
  ([isAdmin, filesSupported, dockerSupported]) => {
    if (!isAdmin && activeTab.value !== 'details') activeTab.value = 'details'
    if (!filesSupported && activeTab.value === 'files') activeTab.value = 'details'
    if (!dockerSupported && activeTab.value === 'docker') activeTab.value = 'details'
  },
)

watch(
  () => props.isAdmin,
  (isAdmin) => {
    dockerStatusRequest += 1
    if (dockerStatusTimer) {
      clearInterval(dockerStatusTimer)
      dockerStatusTimer = null
    }
    dockerStatus.value = null
    void loadDeviceProfile()
    if (isAdmin) {
      void loadDockerStatus()
      startDockerStatusPolling()
    }
  },
)

const remoteDesktopUnavailableReason = computed(() => {
  if (!props.instance.online) return '实例离线，无法连接远程桌面'
  return ''
})

const selectedHistoryRange = computed(() =>
  historyRanges.find((option) => option.value === historyRange.value) || historyRanges[0],
)

const historyWindowTo = computed(() =>
  historyTo.value || props.instance.metrics?.ts || Math.floor(Date.now() / 1000),
)
const historyWindowFrom = computed(() =>
  historyFrom.value || historyWindowTo.value - selectedHistoryRange.value.seconds,
)

const chartMetrics = computed(() => {
  const metrics = new Map(historyMetrics.value.map((metric) => [metric.ts, metric]))
  const latest = props.instance.metrics
  if (latest && latest.ts >= historyWindowFrom.value) metrics.set(latest.ts, latest)
  return [...metrics.values()]
    .filter((metric) => metric.ts >= historyWindowFrom.value && metric.ts <= historyWindowTo.value)
    .sort((left, right) => left.ts - right.ts)
})

const chartDomain = computed(() => ({
  from: historyWindowFrom.value,
  to: historyWindowTo.value,
}))

const cpuHistory = computed(() => chartMetrics.value.map((metric) => ({
  ts: metric.ts,
  value: metric.cpu_percent,
})))

const memoryHistory = computed(() => chartMetrics.value.map((metric) => ({
  ts: metric.ts,
  value: metric.memory_total > 0 ? metricPercent(metric.memory_used, metric.memory_total) : 0,
})))

const diskHistory = computed(() => chartMetrics.value.map((metric) => ({
  ts: metric.ts,
  value: metric.disk_total > 0 ? metricPercent(metric.disk_used, metric.disk_total) : 0,
})))

const gpuHistory = computed(() => chartMetrics.value.map((metric) => ({
  ts: metric.ts,
  value: metric.gpu_percent,
})))

const latencyHistory = computed(() => chartMetrics.value.map((metric) => ({
  ts: metric.ts,
  value: metric.latency_ms,
})))

watch(
  [() => props.instance.id, historyRange],
  () => void loadMetricHistory(),
  { immediate: true },
)

onBeforeUnmount(() => {
  document.removeEventListener('keydown', handleDocumentKeydown)
  historyAbort?.abort()
  dockerStatusRequest += 1
  deviceProfileRequest += 1
  if (dockerStatusTimer) clearInterval(dockerStatusTimer)
})

onMounted(() => {
  document.addEventListener('keydown', handleDocumentKeydown)
})

function instanceName() {
  return props.instance.name || props.instance.hostname || '未命名节点'
}

function instanceCountry() {
  return getCountryOption(props.instance.country_code)?.name
    || props.instance.country
    || '未设置国家'
}

function formatLatency(value: number | null | undefined) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '未知'
  return `${value.toFixed(value >= 10 ? 0 : 1)} ms`
}

function closeImplicitly() {
  if (activeTab.value === 'details' || activeTab.value === 'actions') emit('close')
}

function handleDocumentKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape' || event.defaultPrevented) return
  if (activeTab.value !== 'details' && activeTab.value !== 'actions') return
  event.preventDefault()
  emit('close')
}

function cpuCoreSummary(physical: number | null, logical: number) {
  if (physical && logical) return `${physical} 核 / ${logical} 线程`
  if (logical) return `${logical} 线程`
  return '未知'
}

function formatFrequency(value: number | null | undefined) {
  if (!value) return '未知'
  return value >= 1000 ? `${(value / 1000).toFixed(2)} GHz` : `${value} MHz`
}

async function loadDeviceProfile() {
  const request = ++deviceProfileRequest
  publicDeviceProfile.value = null
  adminDeviceProfile.value = null
  deviceProfileError.value = ''
  adminDeviceProfileError.value = ''
  deviceProfileLoading.value = true
  try {
    const publicProfile = await api<PublicDeviceProfileResponse>(
      `/api/public/instances/${encodeURIComponent(props.instance.id)}/device-profile`,
    )
    if (request !== deviceProfileRequest) return
    publicDeviceProfile.value = publicProfile
  } catch (error) {
    if (request !== deviceProfileRequest) return
    deviceProfileError.value = error instanceof Error ? error.message : '设备配置读取失败'
  }

  if (props.isAdmin && request === deviceProfileRequest) {
    try {
      const adminProfile = await api<AdminDeviceProfileResponse>(
        `/api/admin/instances/${encodeURIComponent(props.instance.id)}/device-profile`,
      )
      if (request !== deviceProfileRequest) return
      adminDeviceProfile.value = adminProfile
    } catch (error) {
      if (request !== deviceProfileRequest) return
      adminDeviceProfileError.value = error instanceof Error ? error.message : '完整设备配置读取失败'
    }
  }
  if (request === deviceProfileRequest) deviceProfileLoading.value = false
}

async function loadMetricHistory() {
  historyAbort?.abort()
  const controller = new AbortController()
  historyAbort = controller
  const range = selectedHistoryRange.value
  const to = Math.floor(Date.now() / 1000)
  const from = to - range.seconds
  historyFrom.value = from
  historyTo.value = to
  historyMetrics.value = []
  historyError.value = ''
  historyLoading.value = true

  const params = new URLSearchParams({
    from: String(from),
    to: String(to),
    bucket_seconds: String(range.bucketSeconds),
    limit: '500',
  })
  try {
    const metrics = await api<Metric[]>(
      `/api/public/instances/${encodeURIComponent(props.instance.id)}/metrics?${params}`,
      { signal: controller.signal },
    )
    if (historyAbort !== controller || controller.signal.aborted) return
    historyMetrics.value = metrics
  } catch (error) {
    if (historyAbort !== controller || controller.signal.aborted) return
    historyError.value = error instanceof Error ? error.message : '历史指标读取失败'
  } finally {
    if (historyAbort === controller) {
      historyAbort = null
      historyLoading.value = false
    }
  }
}

async function loadDockerStatus() {
  if (!props.isAdmin) return
  const request = ++dockerStatusRequest
  try {
    const status = await getDockerStatus(props.instance.id)
    if (request === dockerStatusRequest) dockerStatus.value = status
  } catch {}
}

function startDockerStatusPolling() {
  if (!props.isAdmin) return
  if (dockerStatusTimer) clearInterval(dockerStatusTimer)
  dockerStatusTimer = setInterval(() => void loadDockerStatus(), 15_000)
}
</script>

<template>
  <div class="modal-backdrop instance-detail-backdrop" @click.self="closeImplicitly">
    <section class="modal instance-detail-modal" role="dialog" aria-modal="true" aria-labelledby="instance-detail-title">
      <header class="instance-detail-header">
        <div class="instance-detail-identity">
          <OperatingSystemLogo class="server-icon detail-server-icon" :os="instance.os" />
          <div>
            <div class="instance-detail-title-line">
              <h2 id="instance-detail-title">{{ instanceName() }}</h2>
              <span :class="['status-badge', { online: instance.online }]">
                <i></i>{{ instance.online ? '在线' : '离线' }}
              </span>
            </div>
            <p>
              <span><CountryFlag :code="instance.country_code" :name="instanceCountry()" />{{ instanceCountry() }}</span>
              <span>{{ instance.os }}/{{ instance.arch }}</span>
              <span>{{ instance.hostname }}</span>
            </p>
          </div>
        </div>
        <button class="icon-button subtle" type="button" title="关闭" aria-label="关闭实例详情" @click="emit('close')">
          <X :size="17" />
        </button>
      </header>

      <nav class="instance-detail-tabs" role="tablist" aria-label="实例面板">
        <button
          :class="{ active: activeTab === 'details' }"
          type="button"
          role="tab"
          :aria-selected="activeTab === 'details'"
          @click="activeTab = 'details'"
        >
          <Info :size="15" />详情
        </button>
        <button
          v-if="isAdmin"
          :class="{ active: activeTab === 'actions' }"
          type="button"
          role="tab"
          :aria-selected="activeTab === 'actions'"
          @click="activeTab = 'actions'"
        >
          <FileCog :size="15" />操作
        </button>
        <button
          v-if="isAdmin && supportsFiles"
          :class="{ active: activeTab === 'files' }"
          type="button"
          role="tab"
          :aria-selected="activeTab === 'files'"
          @click="activeTab = 'files'"
        >
          <UploadCloud :size="15" />文件
        </button>
        <button
          v-if="isAdmin && supportsDocker"
          :class="{ active: activeTab === 'docker' }"
          type="button"
          role="tab"
          :aria-selected="activeTab === 'docker'"
          @click="activeTab = 'docker'"
        >
          <Box :size="15" />容器
        </button>
      </nav>

      <div :class="['instance-detail-content', { 'files-active': activeTab === 'files' || activeTab === 'docker' }]">
        <section v-if="activeTab === 'details'" class="instance-overview" role="tabpanel">
          <div class="metric-history-section">
            <header class="metric-history-toolbar">
              <div>
                <Activity :size="16" />
                <div><h3>资源趋势</h3><p>悬停折线可查看对应时间点</p></div>
              </div>
              <div class="metric-history-ranges" role="group" aria-label="历史指标时间范围">
                <button
                  v-for="option in historyRanges"
                  :key="option.value"
                  :class="{ active: historyRange === option.value }"
                  type="button"
                  :aria-pressed="historyRange === option.value"
                  @click="historyRange = option.value"
                >
                  {{ option.label }}
                </button>
              </div>
            </header>

            <div v-if="historyError" class="metric-history-error" role="alert">
              <span>{{ historyError }}</span>
              <button type="button" @click="loadMetricHistory">重试</button>
            </div>

            <div class="detail-metrics">
              <MetricHistoryChart
                title="CPU"
                :points="cpuHistory"
                :from="chartDomain.from"
                :to="chartDomain.to"
                color="#58d4b1"
                :loading="historyLoading"
              >
                <template #icon><Cpu :size="16" /></template>
              </MetricHistoryChart>
              <MetricHistoryChart
                title="内存"
                :points="memoryHistory"
                :from="chartDomain.from"
                :to="chartDomain.to"
                color="#55b8cf"
                :loading="historyLoading"
              >
                <template #icon><MemoryStick :size="16" /></template>
              </MetricHistoryChart>
              <MetricHistoryChart
                title="磁盘"
                :points="diskHistory"
                :from="chartDomain.from"
                :to="chartDomain.to"
                color="#e5ae54"
                :loading="historyLoading"
              >
                <template #icon><HardDrive :size="16" /></template>
              </MetricHistoryChart>
              <MetricHistoryChart
                title="GPU"
                :points="gpuHistory"
                :from="chartDomain.from"
                :to="chartDomain.to"
                color="#aaa5dc"
                :loading="historyLoading"
              >
                <template #icon><Zap :size="16" /></template>
              </MetricHistoryChart>
              <MetricHistoryChart
                class="latency-history-chart"
                title="通信延迟"
                :points="latencyHistory"
                :from="chartDomain.from"
                :to="chartDomain.to"
                color="#df8f72"
                :loading="historyLoading"
                value-type="milliseconds"
              >
                <template #icon><Timer :size="16" /></template>
              </MetricHistoryChart>
            </div>
          </div>

          <div class="detail-section">
            <header><Server :size="16" /><h3>实例资料</h3></header>
            <dl class="detail-grid">
              <div><dt>实例 ID</dt><dd :title="instance.id">{{ instance.id }}</dd></div>
              <div><dt>主机名</dt><dd>{{ instance.hostname || '未知' }}</dd></div>
              <div><dt>操作系统</dt><dd>{{ instance.os || '未知' }}</dd></div>
              <div><dt>架构</dt><dd>{{ instance.arch || '未知' }}</dd></div>
              <div><dt>Agent 版本</dt><dd>{{ instance.agent_version || '未知' }}</dd></div>
              <div><dt>地区</dt><dd>{{ instance.region || instanceCountry() }}</dd></div>
              <div><dt>首次接入</dt><dd>{{ formatTime(instance.first_seen) }}</dd></div>
              <div><dt>最后上报</dt><dd>{{ formatTime(instance.last_seen) }}</dd></div>
            </dl>
            <div v-if="instance.remark" class="detail-remark">
              <MapPin :size="15" /><span>{{ instance.remark }}</span>
            </div>
          </div>

          <div class="detail-section device-profile-section">
            <header>
              <Cpu :size="16" />
              <h3>硬件配置</h3>
              <small v-if="publicDeviceProfile?.updated_at">更新于 {{ formatTime(publicDeviceProfile.updated_at) }}</small>
            </header>
            <div v-if="deviceProfileLoading" class="device-profile-state">
              <LoaderCircle class="spin" :size="17" />正在读取设备配置
            </div>
            <div v-else-if="deviceProfileError" class="device-profile-state error" role="alert">
              <span>{{ deviceProfileError }}</span>
              <button class="text-button" type="button" @click="loadDeviceProfile">
                <RefreshCw :size="13" />重试
              </button>
            </div>
            <div v-else-if="!publicDeviceProfile?.profile" class="device-profile-state">
              <span>当前 Agent 尚未上报设备配置</span>
              <button class="text-button" type="button" @click="loadDeviceProfile">
                <RefreshCw :size="13" />刷新
              </button>
            </div>
            <dl v-else class="detail-grid device-summary-grid">
              <div>
                <dt><Cpu :size="13" />CPU</dt>
                <dd :title="publicDeviceProfile.profile.cpu_model">{{ publicDeviceProfile.profile.cpu_model || '未知' }}</dd>
                <small>{{ cpuCoreSummary(publicDeviceProfile.profile.physical_cores, publicDeviceProfile.profile.logical_cores) }}</small>
              </div>
              <div>
                <dt><MemoryStick :size="13" />内存</dt>
                <dd>{{ formatBytes(publicDeviceProfile.profile.memory_total) }}</dd>
              </div>
              <div>
                <dt><HardDrive :size="13" />存储容量</dt>
                <dd>{{ formatBytes(publicDeviceProfile.profile.storage_total) }}</dd>
              </div>
              <div>
                <dt><Monitor :size="13" />系统</dt>
                <dd>{{ publicDeviceProfile.profile.os_name || '未知' }} {{ publicDeviceProfile.profile.os_version }}</dd>
                <small>{{ publicDeviceProfile.profile.architecture || '未知架构' }}</small>
              </div>
              <div class="device-gpu-summary">
                <dt><Zap :size="13" />GPU</dt>
                <dd v-if="publicDeviceProfile.profile.gpus.length" class="device-value-list">
                  <span v-for="gpu in publicDeviceProfile.profile.gpus" :key="`${gpu.name}-${gpu.memory_total}`">
                    <strong>{{ gpu.name }}</strong>
                    <small>{{ gpu.memory_total ? formatBytes(gpu.memory_total) : '显存未知' }}</small>
                  </span>
                </dd>
                <dd v-else>未检测到 GPU</dd>
              </div>
            </dl>
          </div>

          <div v-if="isAdmin" class="detail-section device-admin-section">
            <header><Network :size="16" /><h3>设备与网络详情</h3></header>
            <div v-if="adminDeviceProfileError" class="device-profile-state error" role="alert">
              <span>{{ adminDeviceProfileError }}</span>
              <button class="text-button" type="button" @click="loadDeviceProfile">
                <RefreshCw :size="13" />重试
              </button>
            </div>
            <template v-else-if="adminDeviceProfile?.profile">
              <dl class="detail-grid device-admin-grid">
                <div><dt>内核版本</dt><dd :title="adminDeviceProfile.profile.system.kernel_version">{{ adminDeviceProfile.profile.system.kernel_version || '未知' }}</dd></div>
                <div><dt>CPU 厂商</dt><dd>{{ adminDeviceProfile.profile.cpu.vendor || '未知' }}</dd></div>
                <div><dt>CPU 频率</dt><dd>{{ formatFrequency(adminDeviceProfile.profile.cpu.frequency_mhz) }}</dd></div>
                <div><dt>连接来源 IP</dt><dd>{{ adminDeviceProfile.observed_ip || '未知' }}</dd></div>
              </dl>

              <div class="device-inventory">
                <div class="device-inventory-group">
                  <h4><HardDrive :size="14" />磁盘</h4>
                  <div v-if="adminDeviceProfile.profile.disks.length" class="device-inventory-list">
                    <div v-for="disk in adminDeviceProfile.profile.disks" :key="`${disk.name}-${disk.mount_point}`" class="device-inventory-row">
                      <strong :title="disk.name">{{ disk.name || '未命名磁盘' }}</strong>
                      <span :title="disk.mount_point">{{ disk.mount_point || '无挂载点' }}</span>
                      <span>{{ disk.file_system || disk.kind || '未知类型' }}</span>
                      <span>{{ formatBytes(disk.total_bytes) }}</span>
                    </div>
                  </div>
                  <p v-else class="device-inventory-empty">未检测到磁盘明细</p>
                </div>

                <div class="device-inventory-group">
                  <h4><Network :size="14" />网络接口</h4>
                  <div v-if="adminDeviceProfile.profile.network_interfaces.length" class="device-network-list">
                    <div v-for="networkInterface in adminDeviceProfile.profile.network_interfaces" :key="networkInterface.name" class="device-network-row">
                      <div>
                        <strong>{{ networkInterface.name }}</strong>
                        <small>{{ networkInterface.mac_address || '无 MAC 地址' }}</small>
                      </div>
                      <div class="device-address-list">
                        <span v-for="address in networkInterface.ipv4" :key="`v4-${address}`">{{ address }}</span>
                        <span v-for="address in networkInterface.ipv6" :key="`v6-${address}`">{{ address }}</span>
                        <span v-if="!networkInterface.ipv4.length && !networkInterface.ipv6.length">无 IP 地址</span>
                      </div>
                    </div>
                  </div>
                  <p v-else class="device-inventory-empty">未检测到网络接口</p>
                </div>
              </div>
            </template>
            <div v-else-if="!deviceProfileLoading" class="device-profile-state">暂无完整设备资料</div>
          </div>

          <div class="detail-section">
            <header><Activity :size="16" /><h3>运行与网络</h3></header>
            <dl class="detail-grid detail-runtime-grid">
              <div><dt><Clock3 :size="13" />运行时长</dt><dd>{{ formatDuration(instance.metrics?.uptime_seconds) }}</dd></div>
              <div><dt><Network :size="13" />网络接收</dt><dd>{{ formatBytes(instance.metrics?.network_rx) }}</dd></div>
              <div><dt><Radio :size="13" />网络发送</dt><dd>{{ formatBytes(instance.metrics?.network_tx) }}</dd></div>
              <div><dt><Gauge :size="13" />系统负载</dt><dd>{{ instance.metrics?.load_average?.toFixed(2) || '未知' }}</dd></div>
              <div><dt><Box :size="13" />GPU 显存</dt><dd>{{ formatBytes(instance.metrics?.gpu_memory_used) }} / {{ formatBytes(instance.metrics?.gpu_memory_total) }}</dd></div>
              <div><dt><Timer :size="13" />最近通信延迟</dt><dd>{{ formatLatency(instance.metrics?.latency_ms) }}</dd></div>
              <div><dt>{{ instance.online ? '连接状态' : '离线状态' }}</dt><dd>{{ instance.online ? 'WebSocket 已连接' : '等待 Agent 重连' }}</dd></div>
            </dl>
          </div>
        </section>

        <section v-else-if="activeTab === 'actions'" class="instance-operations" role="tabpanel">
          <div class="operation-section">
            <header>
              <div><h3>实例操作</h3><p>管理资料，或进入交互式终端与远程桌面</p></div>
              <span :class="['operation-connection', { online: instance.online }]">
                <Wifi v-if="instance.online" :size="14" />
                <WifiOff v-else :size="14" />
                {{ instance.online ? '实例可操作' : '实例离线' }}
              </span>
            </header>
            <div class="operation-command-grid">
              <button type="button" :disabled="loading" @click="emit('edit', instance)">
                <span><Pencil :size="18" /></span>
                <strong>编辑资料</strong>
                <small>名称、地区与备注</small>
              </button>
              <button type="button" :disabled="!instance.online || loading" @click="emit('terminal', instance)">
                <span><Terminal :size="18" /></span>
                <strong>Web 终端</strong>
                <small>打开交互式 Shell</small>
              </button>
              <button
                v-if="supportsRemoteDesktop"
                type="button"
                :disabled="Boolean(remoteDesktopUnavailableReason) || loading"
                :title="remoteDesktopUnavailableReason || '在浏览器中控制 Windows 桌面'"
                @click="emit('remoteDesktop', instance)"
              >
                <span><Monitor :size="18" /></span>
                <strong>远程桌面</strong>
                <small>{{ remoteDesktopUnavailableReason || '浏览器内操作 Windows' }}</small>
              </button>
            </div>
          </div>

          <div class="operation-section">
            <header><div><h3>快捷命令</h3><p>执行管理员配置的白名单命令</p></div></header>
            <div v-if="commands.length" class="detail-command-list">
              <button
                v-for="command in commands"
                :key="command.id"
                type="button"
                :disabled="!instance.online || loading"
                :title="command.command"
                @click="emit('runCommand', instance, command)"
              >
                <Play :size="14" />{{ command.name }}
              </button>
            </div>
            <p v-else class="operation-empty">暂无启用的快捷命令</p>
          </div>

          <div class="operation-section danger-zone">
            <header><div><h3>危险操作</h3><p>停用或永久移除当前实例</p></div><ShieldAlert :size="18" /></header>
            <div class="danger-actions">
              <button type="button" :disabled="loading" @click="emit('disable', instance)">
                <Pause :size="15" />停用实例
              </button>
              <button type="button" :disabled="loading" @click="emit('delete', instance)">
                <Trash2 :size="15" />永久删除
              </button>
            </div>
          </div>
        </section>

        <section v-else-if="activeTab === 'files'" class="instance-files-tab" role="tabpanel">
          <div v-if="!instance.online" class="file-unavailable">
            <WifiOff :size="30" />
            <strong>实例当前离线</strong>
            <span>Agent 重新连接后才能浏览和传输文件。</span>
          </div>
          <FileManagerPanel v-else :instance="instance" />
        </section>
        <section v-else-if="dockerStatus" class="instance-docker-tab" role="tabpanel">
          <DockerManagerPanel
            :instance="instance"
            :status="dockerStatus"
            @status="dockerStatus = $event"
          />
        </section>
      </div>
    </section>
  </div>
</template>
