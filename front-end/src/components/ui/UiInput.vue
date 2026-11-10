<script setup lang="ts">
import { ref } from 'vue'

const [model, modifiers] = defineModel<string>({ set: (value) => modifiers.trim ? value.trim() : value })
const element = ref<HTMLInputElement | null>(null)
defineExpose({ focus: (options?: FocusOptions) => element.value?.focus(options), select: () => element.value?.select() })
</script>

<template>
  <input ref="element" v-model="model" class="ui-input" />
</template>
