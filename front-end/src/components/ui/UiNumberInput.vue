<script setup lang="ts" generic="T extends number | null | undefined">
import { computed, ref } from 'vue'
import { Minus, Plus } from '@lucide/vue'
import { stepNumber } from '../../utils/controls'

defineOptions({ inheritAttrs: false })
const props = withDefaults(defineProps<{
  modelValue?: T
  min?: number | string
  max?: number | string
  step?: number | string
  disabled?: boolean
}>(), { step: 1 })
const emit = defineEmits<{ 'update:modelValue': [value: T]; change: [event: Event] }>()
const element = ref<HTMLInputElement | null>(null)
const atMin = computed(() => props.min !== undefined && props.modelValue != null && props.modelValue <= Number(props.min))
const atMax = computed(() => props.max !== undefined && props.modelValue != null && props.modelValue >= Number(props.max))

function input(event: Event) {
  const target = event.target as HTMLInputElement
  emit('update:modelValue', (target.value === '' ? null : target.valueAsNumber) as T)
}

function stepBy(direction: number) {
  if (!element.value) return
  element.value.value = String(stepNumber(props.modelValue, direction, Number(props.step), Number(props.min ?? -Infinity), Number(props.max ?? Infinity)))
  element.value.dispatchEvent(new Event('input', { bubbles: true }))
  element.value.dispatchEvent(new Event('change', { bubbles: true }))
  element.value.focus()
}
</script>

<template>
  <span class="ui-number ui-control" :class="{ 'is-disabled': disabled }">
    <input ref="element" v-bind="$attrs" type="number" :value="modelValue" :min="min" :max="max" :step="step" :disabled="disabled" @input="input" @change="emit('change', $event)" />
    <span class="ui-number-actions">
      <button type="button" tabindex="-1" aria-label="减小数值" :disabled="disabled || atMin" @click="stepBy(-1)"><Minus :size="12" /></button>
      <button type="button" tabindex="-1" aria-label="增大数值" :disabled="disabled || atMax" @click="stepBy(1)"><Plus :size="12" /></button>
    </span>
  </span>
</template>
