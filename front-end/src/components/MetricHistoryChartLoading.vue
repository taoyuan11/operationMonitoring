<script setup lang="ts">
import { LoaderCircle } from '@lucide/vue'
</script>

<template>
  <article class="metric-history-card" aria-busy="true" aria-label="正在准备资源趋势图表">
    <div class="metric-chart-shell">
      <div class="metric-chart-state">
        <LoaderCircle class="spin" :size="17" />正在准备图表
      </div>
    </div>
  </article>
</template>
