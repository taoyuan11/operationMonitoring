<script setup lang="ts">
import { computed } from 'vue'
const props = withDefaults(defineProps<{ value: number; max?: number | string }>(), { max: 100 })
const percent = computed(() => Math.min(100, Math.max(0, props.value / Math.max(1, Number(props.max)) * 100)))
</script>

<template>
  <div class="ui-progress" role="progressbar" aria-label="上传进度" :aria-valuenow="percent" :aria-valuemin="0" :aria-valuemax="100">
    <span :style="{ width: `${percent}%` }" />
  </div>
</template>
