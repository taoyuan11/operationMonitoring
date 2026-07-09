<script setup lang="ts">
import { formatPercent } from '../utils/format'

defineProps<{
  total: number
  online: number
  avgCpu: number
  avgMemory: number
}>()
</script>

<template>
  <section class="summary-band">
    <div class="summary-item">
      <span>实例总数</span>
      <strong>{{ total }}</strong>
    </div>
    <div class="summary-item">
      <span>在线实例</span>
      <strong>{{ online }}</strong>
    </div>
    <div class="summary-item">
      <span>平均 CPU</span>
      <strong>{{ formatPercent(avgCpu) }}</strong>
    </div>
    <div class="summary-item">
      <span>平均内存</span>
      <strong>{{ formatPercent(avgMemory) }}</strong>
    </div>
  </section>
</template>
