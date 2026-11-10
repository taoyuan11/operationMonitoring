<script setup lang="ts" generic="T extends string | number">
import { computed, nextTick, ref, useId, watch } from 'vue'
import { Check, ChevronDown, Search } from '@lucide/vue'
import { useControlPopover } from '../../composables/useControlPopover'
import { nextEnabledOption, type SelectOption } from '../../utils/controls'

defineOptions({ inheritAttrs: false })
const props = withDefaults(defineProps<{
  modelValue?: T
  options: readonly SelectOption[]
  placeholder?: string
  disabled?: boolean
  required?: boolean
  name?: string
}>(), { placeholder: '请选择' })
const emit = defineEmits<{ 'update:modelValue': [value: T]; change: [value: T] }>()
const id = useId()
const trigger = ref<HTMLButtonElement | null>(null)
const panel = ref<HTMLElement | null>(null)
const search = ref<HTMLInputElement | null>(null)
const proxy = ref<HTMLInputElement | null>(null)
const query = ref('')
const active = ref(-1)
const { open, style, target, close } = useControlPopover(trigger, panel)
const selected = computed(() => props.options.find((option) => option.value === props.modelValue))
const filtered = computed(() => props.options.filter((option) => option.label.toLocaleLowerCase().includes(query.value.trim().toLocaleLowerCase())))
const searchable = computed(() => props.options.length > 8)

watch(query, () => { active.value = nextEnabledOption(filtered.value, -1, 1) })
watch(() => props.disabled, (disabled) => { if (disabled) close() })
watch(() => props.modelValue, () => nextTick(() => proxy.value?.dispatchEvent(new Event('input', { bubbles: true }))))

async function show(last = false, forceEdge = false) {
  if (props.disabled) return
  query.value = ''
  active.value = props.options.findIndex((option) => option.value === props.modelValue && !option.disabled)
  if (forceEdge || active.value < 0) active.value = nextEnabledOption(props.options, last ? 0 : -1, last ? -1 : 1)
  open.value = true
  await nextTick()
  if (searchable.value) search.value?.focus()
  reveal()
}

function reveal() {
  void nextTick(() => panel.value?.querySelector(`#${id}-option-${active.value}`)?.scrollIntoView({ block: 'nearest' }))
}

function choose(index: number) {
  const option = filtered.value[index]
  if (!option || option.disabled) return
  emit('update:modelValue', option.value as T)
  emit('change', option.value as T)
  close(true)
}

let typed = ''
let lastTyped = 0
function keydown(event: KeyboardEvent) {
  if (event.isComposing) return
  if (event.key === 'Tab') { close(true); return }
  if (['ArrowDown', 'ArrowUp', 'Home', 'End', 'Enter', ' '].includes(event.key) && !(event.target === search.value && event.key === ' ')) {
    event.preventDefault()
    if (!open.value) { void show(event.key === 'ArrowUp' || event.key === 'End', event.key === 'Home' || event.key === 'End'); return }
    if (event.key === 'Enter' || event.key === ' ') { choose(active.value); return }
    active.value = nextEnabledOption(filtered.value, event.key === 'Home' ? -1 : event.key === 'End' ? 0 : active.value, event.key === 'ArrowUp' || event.key === 'End' ? -1 : 1)
    reveal()
  } else if (!searchable.value && event.key.length === 1 && !event.ctrlKey && !event.metaKey) {
    event.preventDefault()
    typed = Date.now() - lastTyped > 700 ? event.key : typed + event.key
    lastTyped = Date.now()
    if (!open.value) void show()
    const index = filtered.value.findIndex((option) => !option.disabled && option.label.toLocaleLowerCase().startsWith(typed.toLocaleLowerCase()))
    if (index >= 0) { active.value = index; reveal() }
  }
}
</script>

<template>
  <span class="ui-select ui-control" :class="{ 'is-open': open, 'is-disabled': disabled }">
    <button ref="trigger" v-bind="$attrs" type="button" class="ui-select-trigger" role="combobox" aria-haspopup="listbox" :aria-controls="`${id}-list`" :aria-expanded="open" :aria-required="required" :aria-activedescendant="open && active >= 0 ? `${id}-option-${active}` : undefined" :disabled="disabled" @click="open ? close() : show()" @keydown="keydown">
      <span :class="{ 'ui-placeholder': !selected }">{{ selected?.label ?? placeholder }}</span><ChevronDown :size="14" aria-hidden="true" />
    </button>
    <input ref="proxy" class="ui-validation-proxy" tabindex="-1" aria-hidden="true" :value="selected && !selected.disabled ? modelValue : ''" :name="name" :required="required" :disabled="disabled" @focus="trigger?.focus()" />
    <Teleport :to="target">
      <div v-if="open" ref="panel" class="ui-popover ui-select-menu" :style="style" @keydown="keydown" @pointerdown.stop>
        <div v-if="searchable" class="ui-select-search"><Search :size="14" /><input ref="search" v-model="query" placeholder="搜索选项…" role="combobox" aria-label="搜索选项" aria-expanded="true" :aria-controls="`${id}-list`" :aria-activedescendant="active >= 0 ? `${id}-option-${active}` : undefined" /></div>
        <div :id="`${id}-list`" role="listbox" aria-label="可选项" class="ui-select-options">
          <div v-for="(option, index) in filtered" :id="`${id}-option-${index}`" :key="option.value" class="ui-select-option" :class="{ 'is-active': active === index, 'is-selected': option.value === modelValue }" role="option" :aria-selected="option.value === modelValue" :aria-disabled="option.disabled || undefined" @pointermove="!option.disabled && (active = index)" @mousedown.prevent @click.stop="choose(index)">
            <span>{{ option.label }}</span><Check v-if="option.value === modelValue" :size="14" aria-hidden="true" />
          </div>
          <p v-if="!filtered.length" class="ui-select-empty">没有匹配的选项</p>
        </div>
        <div v-if="searchable" class="ui-select-hint">{{ filtered.length }} 个选项 <span>↑ ↓ 选择 · Enter 确认</span></div>
      </div>
    </Teleport>
  </span>
</template>
