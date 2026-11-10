<script setup lang="ts">
import { computed, nextTick, ref, useId, watch } from 'vue'
import { CalendarDays, ChevronLeft, ChevronRight, Clock3 } from '@lucide/vue'
import { useControlPopover } from '../../composables/useControlPopover'
import { calendarDays, localDateTime } from '../../utils/controls'
import UiNumberInput from './UiNumberInput.vue'

defineOptions({ inheritAttrs: false })
const props = withDefaults(defineProps<{ modelValue?: string; required?: boolean; disabled?: boolean; name?: string }>(), { modelValue: '' })
const emit = defineEmits<{ 'update:modelValue': [value: string]; change: [value: string] }>()
const id = useId()
const trigger = ref<HTMLButtonElement | null>(null)
const panel = ref<HTMLElement | null>(null)
const proxy = ref<HTMLInputElement | null>(null)
const { open, style, target, close, position } = useControlPopover(trigger, panel, 304)
const draft = ref(new Date())
const view = ref(new Date())
const hour = ref<number | null>(0)
const minute = ref<number | null>(0)
const year = ref<number | null>(new Date().getFullYear())
const days = computed(() => calendarDays(view.value.getFullYear(), view.value.getMonth()))
const dayKey = (date: Date) => localDateTime(date).slice(0, 10)
const draftVisible = computed(() => days.value.some((date) => dayKey(date) === dayKey(draft.value)))
const today = dayKey(new Date())
const display = computed(() => props.modelValue ? props.modelValue.replace('T', ' ').replaceAll('-', '/') : '选择日期与时间')
const validTime = computed(() => hour.value != null && minute.value != null && Number.isInteger(hour.value) && Number.isInteger(minute.value) && hour.value >= 0 && hour.value <= 23 && minute.value >= 0 && minute.value <= 59)

watch(() => props.disabled, (disabled) => { if (disabled) close() })
watch(() => props.modelValue, () => nextTick(() => proxy.value?.dispatchEvent(new Event('input', { bubbles: true }))))

async function show() {
  if (props.disabled) return
  const parsed = props.modelValue ? new Date(props.modelValue) : new Date()
  draft.value = Number.isNaN(parsed.getTime()) ? new Date() : parsed
  view.value = new Date(draft.value.getFullYear(), draft.value.getMonth(), 1)
  year.value = view.value.getFullYear()
  hour.value = draft.value.getHours()
  minute.value = draft.value.getMinutes()
  open.value = true
  await nextTick()
  focusDay()
}

function focusDay() {
  void nextTick(() => panel.value?.querySelector<HTMLButtonElement>(`[data-day="${dayKey(draft.value)}"]`)?.focus())
}

function moveMonth(delta: number) {
  view.value = new Date(view.value.getFullYear(), view.value.getMonth() + delta, 1)
  year.value = view.value.getFullYear()
  void nextTick(position)
}

function changeYear() {
  if (year.value && year.value >= 100 && year.value <= 9999) view.value = new Date(year.value, view.value.getMonth(), 1)
  else year.value = view.value.getFullYear()
}

function chooseDay(date: Date) {
  draft.value = new Date(date)
  if (date.getMonth() !== view.value.getMonth()) {
    view.value = new Date(date.getFullYear(), date.getMonth(), 1)
    year.value = date.getFullYear()
  }
}

function calendarKey(event: KeyboardEvent, date: Date) {
  const next = new Date(date)
  const delta: Record<string, number> = { ArrowLeft: -1, ArrowRight: 1, ArrowUp: -7, ArrowDown: 7, Home: -((date.getDay() + 6) % 7), End: 6 - ((date.getDay() + 6) % 7) }
  if (event.key in delta) next.setDate(next.getDate() + delta[event.key]!)
  else if (event.key === 'PageUp' || event.key === 'PageDown') next.setMonth(next.getMonth() + (event.key === 'PageUp' ? -1 : 1), 1)
  else return
  event.preventDefault()
  chooseDay(next)
  focusDay()
}

function commit(value: string) {
  emit('update:modelValue', value)
  emit('change', value)
  close(true)
}

function apply() {
  if (!validTime.value) return
  const date = new Date(draft.value)
  date.setHours(hour.value!, minute.value!, 0, 0)
  commit(localDateTime(date))
}

function useNow() {
  const now = new Date()
  chooseDay(now)
  hour.value = now.getHours()
  minute.value = now.getMinutes()
  focusDay()
}

function tab(event: KeyboardEvent) {
  if (event.key !== 'Tab') return
  const items = Array.from(panel.value?.querySelectorAll<HTMLElement>('button:not(:disabled):not([tabindex="-1"]), input:not(:disabled)') ?? [])
  const first = items[0]
  const last = items.at(-1)
  if (event.shiftKey && document.activeElement === first) { event.preventDefault(); last?.focus() }
  else if (!event.shiftKey && document.activeElement === last) { event.preventDefault(); first?.focus() }
}
</script>

<template>
  <span class="ui-date ui-control">
    <button ref="trigger" v-bind="$attrs" type="button" class="ui-date-trigger" aria-haspopup="dialog" :aria-expanded="open" :aria-controls="`${id}-calendar`" :disabled="disabled" @click="open ? close() : show()">
      <CalendarDays :size="15" aria-hidden="true" /><span :class="{ 'ui-placeholder': !modelValue }">{{ display }}</span>
    </button>
    <input ref="proxy" class="ui-validation-proxy" tabindex="-1" aria-hidden="true" :value="modelValue" :required="required" :disabled="disabled" :name="name" @focus="trigger?.focus()" />
    <Teleport :to="target">
      <section v-if="open" :id="`${id}-calendar`" ref="panel" class="ui-popover ui-calendar" :style="style" role="dialog" aria-modal="true" aria-label="选择日期与时间" @keydown="tab" @click.stop>
        <header class="ui-calendar-header">
          <button type="button" aria-label="上个月" @click="moveMonth(-1)"><ChevronLeft :size="16" /></button>
          <span><input v-model.number="year" class="ui-calendar-year" aria-label="年份" inputmode="numeric" maxlength="4" @change="changeYear" @keydown.enter.prevent="changeYear" />年 {{ view.getMonth() + 1 }} 月</span>
          <button type="button" aria-label="下个月" @click="moveMonth(1)"><ChevronRight :size="16" /></button>
        </header>
        <div class="ui-calendar-week" aria-hidden="true"><span v-for="day in ['一', '二', '三', '四', '五', '六', '日']" :key="day">{{ day }}</span></div>
        <div class="ui-calendar-grid" role="group" aria-label="日期">
          <button v-for="day in days" :key="dayKey(day)" type="button" :data-day="dayKey(day)" :class="{ 'is-outside': day.getMonth() !== view.getMonth(), 'is-today': dayKey(day) === today, 'is-selected': dayKey(day) === dayKey(draft) }" :aria-label="`${day.getFullYear()}年${day.getMonth() + 1}月${day.getDate()}日`" :aria-pressed="dayKey(day) === dayKey(draft)" :aria-current="dayKey(day) === today ? 'date' : undefined" :tabindex="dayKey(day) === dayKey(draft) || (day.getDate() === 1 && day.getMonth() === view.getMonth() && !draftVisible) ? 0 : -1" @click="chooseDay(day)" @keydown="calendarKey($event, day)">{{ day.getDate() }}</button>
        </div>
        <div class="ui-calendar-time"><span><Clock3 :size="14" />时间</span><UiNumberInput v-model="hour" aria-label="小时" :min="0" :max="23" /><b>:</b><UiNumberInput v-model="minute" aria-label="分钟" :min="0" :max="59" /></div>
        <footer class="ui-picker-footer"><button v-if="!required" type="button" class="ui-picker-link" @click="commit('')">清除</button><button type="button" class="ui-picker-link" @click="useNow">此刻</button><button type="button" class="ui-picker-apply" :disabled="!validTime" @click="apply">确定</button></footer>
      </section>
    </Teleport>
  </span>
</template>
