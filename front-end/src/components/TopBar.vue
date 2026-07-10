<script setup lang="ts">
import { Activity, ArrowDown, ArrowUp, Clock3, LockKeyhole, LogOut, RefreshCw, ShieldCheck, Wifi } from 'lucide-vue-next'
import { computed } from 'vue'
import { formatBytes } from '../utils/format'

const props = defineProps<{
  isAdmin: boolean
  currentTime: Date
  total: number
  online: number
  totalTraffic: number
  networkRxRate: number
  networkTxRate: number
  refreshing: boolean
}>()

defineEmits<{
  refresh: []
  login: []
  logout: []
}>()

const timeText = computed(() =>
  props.currentTime.toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }),
)

const dateText = computed(() =>
  props.currentTime.toLocaleDateString('zh-CN', {
    month: 'short',
    day: 'numeric',
    weekday: 'short',
  }),
)

const healthText = computed(() => {
  if (props.total === 0) return '等待节点接入'
  if (props.online === props.total) return '全部运行正常'
  if (props.online === 0) return '节点全部离线'
  return `${props.total - props.online} 个节点离线`
})
</script>

<template>
  <header class="topbar">
    <div class="brand">
      <span class="brand-mark"><Activity :size="22" stroke-width="2.2" /></span>
      <div class="brand-copy">
        <span class="eyebrow">Operations console</span>
        <h1>运行监控</h1>
      </div>
    </div>

    <div class="health-pill" :class="{ warning: online !== total && total > 0 }">
      <span class="health-pulse"></span>
      <span>{{ healthText }}</span>
    </div>

    <div class="top-stats">
      <div class="top-stat time-stat">
        <Clock3 :size="16" />
        <span><small>{{ dateText }}</small><strong>{{ timeText }}</strong></span>
      </div>
      <div class="top-stat">
        <Wifi :size="16" />
        <span><small>累计流量</small><strong>{{ formatBytes(totalTraffic) }}</strong></span>
      </div>
      <div class="traffic-rates">
        <span><ArrowUp :size="13" />{{ formatBytes(networkTxRate) }}/s</span>
        <span><ArrowDown :size="13" />{{ formatBytes(networkRxRate) }}/s</span>
      </div>
    </div>

    <div class="top-actions">
      <button class="icon-button" type="button" title="刷新数据" aria-label="刷新监控数据" :disabled="refreshing" @click="$emit('refresh')">
        <RefreshCw :class="{ spin: refreshing }" :size="17" />
      </button>
      <template v-if="isAdmin">
        <span class="session"><ShieldCheck :size="15" />管理模式</span>
        <button class="text-button" type="button" @click="$emit('logout')">
          <LogOut :size="16" />退出
        </button>
      </template>
      <button v-else class="admin-login-button" type="button" @click="$emit('login')">
        <LockKeyhole :size="15" />管理员登录
      </button>
    </div>
  </header>
</template>
