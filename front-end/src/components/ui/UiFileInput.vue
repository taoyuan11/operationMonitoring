<script setup lang="ts">
import { ref } from 'vue'

const element = ref<HTMLInputElement | null>(null)
defineExpose({ click: () => element.value?.click(), focus: () => element.value?.focus() })
</script>

<template>
  <input ref="element" class="ui-file-input" type="file" />
</template>
