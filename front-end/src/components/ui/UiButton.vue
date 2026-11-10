<script setup lang="ts">
import { ref } from 'vue'

withDefaults(defineProps<{ type?: 'button' | 'submit' | 'reset' }>(), { type: 'button' })
const element = ref<HTMLButtonElement | null>(null)
defineExpose({
  focus: (options?: FocusOptions) => element.value?.focus(options),
  getBoundingClientRect: () => element.value!.getBoundingClientRect(),
})
</script>

<template>
  <button ref="element" class="ui-button" :type="type"><slot /></button>
</template>
