<script setup lang="ts">
import { computed } from 'vue'

defineOptions({ inheritAttrs: false })
const props = defineProps<{ modelValue?: boolean | string[]; value?: string; checked?: boolean; indeterminate?: boolean }>()
const emit = defineEmits<{ 'update:modelValue': [value: boolean | string[]]; change: [event: Event] }>()
const selected = computed(() => Array.isArray(props.modelValue)
  ? props.modelValue.includes(props.value ?? '')
  : props.modelValue ?? props.checked ?? false)

function change(event: Event) {
  const checked = (event.target as HTMLInputElement).checked
  emit('update:modelValue', Array.isArray(props.modelValue)
    ? checked ? [...props.modelValue, props.value ?? ''] : props.modelValue.filter((value) => value !== props.value)
    : checked)
  emit('change', event)
}
</script>

<template>
  <input class="ui-checkbox" type="checkbox" v-bind="$attrs" :checked="selected" :value="value" :indeterminate="indeterminate" @change="change" />
</template>
