<script setup lang="ts">
import { Activity, Clock3, Database, Gauge, LogOut, RefreshCw, ShieldCheck, Wifi } from 'lucide-vue-next'
import { computed } from 'vue'
import { formatBytes } from '../utils/format'

const props = defineProps<{
  isAdmin: boolean
  username: string | null
  currentTime: Date
  total: number
  online: number
  totalTraffic: number
  networkRxRate: number
  networkTxRate: number
}>()

defineEmits<{
  refresh: []
  logout: []
}>()

const timeText = computed(() =>
  props.currentTime.toLocaleTimeString([], {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  }),
)
</script>

<template>
  <header class="topbar">
    <div class="brand">
      <span class="brand-mark"><Activity :size="21" /></span>
      <div>
        <h1>公益站运行监控</h1>
      </div>
    </div>
    <div class="top-stats">
      <span class="top-stat active"><Gauge :size="16" />所有</span>
      <span class="top-stat"><Clock3 :size="16" />当前时间 <strong>{{ timeText }}</strong></span>
      <span class="top-stat"><Wifi :size="16" />当前在线 <strong>{{ online }} / {{ total }}</strong></span>
      <span class="top-stat"><Database :size="16" />流量 <strong>{{ formatBytes(totalTraffic) }}</strong></span>
      <span class="top-stat compact">网 ↑ <strong>{{ formatBytes(networkTxRate) }}/s</strong></span>
      <span class="top-stat compact">速 ↓ <strong>{{ formatBytes(networkRxRate) }}/s</strong></span>
    </div>
    <div class="top-actions">
      <button class="icon-button" type="button" title="刷新" @click="$emit('refresh')">
        <RefreshCw :size="18" />
      </button>
      <template v-if="isAdmin">
        <span class="session"><ShieldCheck :size="16" />{{ username }}</span>
        <button class="text-button" type="button" @click="$emit('logout')">
          <LogOut :size="17" />退出
        </button>
      </template>
    </div>
  </header>
</template>
