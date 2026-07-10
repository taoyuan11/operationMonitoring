<script setup lang="ts">
import { Cpu, MemoryStick, Radio, Server } from 'lucide-vue-next'
import { computed } from 'vue'
import { formatPercent } from '../utils/format'

const props = defineProps<{
  total: number
  online: number
  avgCpu: number
  avgMemory: number
}>()

const onlineRate = computed(() => (props.total > 0 ? Math.round((props.online / props.total) * 100) : 0))
</script>

<template>
  <section class="summary-band">
    <article class="summary-card blue">
      <span class="summary-icon"><Server :size="20" /></span>
      <div><span>节点总数</span><strong>{{ total }}</strong></div>
      <small>已批准接入</small>
    </article>
    <article class="summary-card green">
      <span class="summary-icon"><Radio :size="20" /></span>
      <div><span>在线节点</span><strong>{{ online }}</strong></div>
      <small>{{ onlineRate }}% 可用</small>
    </article>
    <article class="summary-card violet">
      <span class="summary-icon"><Cpu :size="20" /></span>
      <div><span>平均 CPU</span><strong>{{ online ? formatPercent(avgCpu) : '—' }}</strong></div>
      <small>在线节点均值</small>
    </article>
    <article class="summary-card amber">
      <span class="summary-icon"><MemoryStick :size="20" /></span>
      <div><span>平均内存</span><strong>{{ online ? formatPercent(avgMemory) : '—' }}</strong></div>
      <small>在线节点均值</small>
    </article>
  </section>
</template>
