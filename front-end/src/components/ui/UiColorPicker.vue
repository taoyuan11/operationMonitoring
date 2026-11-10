<script setup lang="ts">
import { computed, nextTick, ref, useId, watch } from 'vue'
import { Check, Palette } from '@lucide/vue'
import { useControlPopover } from '../../composables/useControlPopover'

defineOptions({ inheritAttrs: false })
const props = defineProps<{ modelValue: string; disabled?: boolean }>()
const emit = defineEmits<{ 'update:modelValue': [value: string]; input: [value: string] }>()
const id = useId()
const trigger = ref<HTMLButtonElement | null>(null)
const panel = ref<HTMLElement | null>(null)
const hexInput = ref<HTMLInputElement | null>(null)
const { open, style, target, close } = useControlPopover(trigger, panel, 280)
const hex = ref('')
const valid = computed(() => /^#[0-9a-f]{6}$/i.test(hex.value))
const colors = ['#3bbf9b', '#14b8a6', '#22c55e', '#84cc16', '#eab308', '#f59e0b', '#f97316', '#ef4444', '#ec7182', '#ec4899', '#d946ef', '#a855f7', '#8b5cf6', '#6366f1', '#3b82f6', '#0ea5e9', '#06b6d4', '#64748b', '#71717a', '#78716c']
watch(() => props.disabled, (disabled) => { if (disabled) close() })

async function show() {
  hex.value = props.modelValue
  open.value = true
  await nextTick()
  hexInput.value?.focus()
  hexInput.value?.select()
}

function apply() {
  if (!valid.value) return
  emit('update:modelValue', hex.value.toLowerCase())
  emit('input', hex.value.toLowerCase())
  close(true)
}

function keydown(event: KeyboardEvent) {
  if (event.key !== 'Tab') return
  const items = Array.from(panel.value?.querySelectorAll<HTMLElement>('button:not(:disabled), input') ?? [])
  if (event.shiftKey && document.activeElement === items[0]) {
    event.preventDefault()
    items.at(-1)?.focus()
  } else if (!event.shiftKey && document.activeElement === items.at(-1)) {
    event.preventDefault()
    items[0]?.focus()
  }
}
</script>

<template>
  <span class="ui-color">
    <button ref="trigger" v-bind="$attrs" type="button" class="ui-color-trigger" aria-haspopup="dialog" :aria-controls="`${id}-palette`" :aria-expanded="open" :disabled="disabled" :style="{ '--color-preview': modelValue }" @click="open ? close() : show()"><span /></button>
    <Teleport :to="target">
      <section v-if="open" :id="`${id}-palette`" ref="panel" class="ui-popover ui-palette" :style="style" role="dialog" aria-modal="true" aria-label="自定义主题色" @click.stop @keydown="keydown">
        <header><Palette :size="15" /><strong>自定义主题色</strong><span>HEX</span></header>
        <div class="ui-color-preview" :style="{ background: valid ? hex : modelValue }"><span>{{ valid ? hex.toUpperCase() : modelValue.toUpperCase() }}</span></div>
        <div class="ui-color-swatches"><button v-for="color in colors" :key="color" type="button" :style="{ background: color }" :aria-label="`选择颜色 ${color}`" :aria-pressed="hex.toLowerCase() === color" @click="hex = color"><Check v-if="hex.toLowerCase() === color" :size="14" /></button></div>
        <label class="ui-color-hex"><span>颜色值</span><input ref="hexInput" v-model="hex" maxlength="7" spellcheck="false" placeholder="#3BBF9B" :aria-invalid="!valid" :aria-describedby="!valid ? `${id}-error` : undefined" @keydown.enter.prevent="apply" /></label>
        <small v-if="!valid" :id="`${id}-error`" class="ui-field-error">请输入 # 和 6 位十六进制颜色值</small>
        <footer class="ui-picker-footer"><button type="button" class="ui-picker-link" @click="close(true)">取消</button><button type="button" class="ui-picker-apply" :disabled="!valid" @click="apply">应用颜色</button></footer>
      </section>
    </Teleport>
  </span>
</template>
