<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, onUnmounted, ref, useId } from 'vue'
import { X } from '@lucide/vue'

const props = withDefaults(defineProps<{
  title: string
  description?: string
  size?: 'medium' | 'wide'
  modal?: boolean
  busy?: boolean
  closeLabel?: string
}>(), {
  description: '',
  size: 'medium',
  modal: true,
  busy: false,
  closeLabel: '关闭',
})

const emit = defineEmits<{
  close: []
}>()

const drawer = ref<HTMLElement | null>(null)
const titleId = `workspace-drawer-title-${useId()}`
const descriptionId = `workspace-drawer-description-${useId()}`
let previouslyFocused: HTMLElement | null = null

const focusableSelector = [
  'a[href]',
  'area[href]',
  'button:not([disabled])',
  'input:not([disabled]):not([type="hidden"])',
  'select:not([disabled])',
  'textarea:not([disabled])',
  '[contenteditable="true"]',
  '[tabindex]:not([tabindex="-1"])',
].join(',')

onMounted(async () => {
  previouslyFocused = document.activeElement instanceof HTMLElement
    ? document.activeElement
    : null
  document.addEventListener('keydown', handleDocumentKeydown)

  await nextTick()
  const autofocusTarget = drawer.value?.querySelector<HTMLElement>('[autofocus]')
  const initialFocusTarget = autofocusTarget || drawer.value
  initialFocusTarget?.focus({ preventScroll: true })
})

onBeforeUnmount(() => {
  document.removeEventListener('keydown', handleDocumentKeydown)
})

onUnmounted(() => {
  if (previouslyFocused?.isConnected) {
    previouslyFocused.focus({ preventScroll: true })
  }
})

function requestClose() {
  if (!props.busy) emit('close')
}

function handleDocumentKeydown(event: KeyboardEvent) {
  if (event.defaultPrevented || event.isComposing || event.key !== 'Escape') return
  event.preventDefault()
  requestClose()
}

function handleDrawerKeydown(event: KeyboardEvent) {
  if (!props.modal || event.key !== 'Tab' || event.defaultPrevented) return

  const focusable = getFocusableElements()
  if (focusable.length === 0) {
    event.preventDefault()
    drawer.value?.focus({ preventScroll: true })
    return
  }

  const first = focusable[0]
  const last = focusable[focusable.length - 1]
  const active = document.activeElement

  if (event.shiftKey && (active === first || !drawer.value?.contains(active))) {
    event.preventDefault()
    last?.focus()
  } else if (!event.shiftKey && (active === last || !drawer.value?.contains(active))) {
    event.preventDefault()
    first?.focus()
  }
}

function getFocusableElements() {
  if (!drawer.value) return []
  return Array.from(drawer.value.querySelectorAll<HTMLElement>(focusableSelector))
    .filter((element) => element.getClientRects().length > 0 && element.getAttribute('aria-hidden') !== 'true')
}
</script>

<template>
  <div
    :class="['workspace-drawer-layer', { 'is-modal': modal }]"
    @click.self="requestClose"
  >
    <section
      ref="drawer"
      :class="['workspace-drawer', `workspace-drawer-${size}`]"
      role="dialog"
      :aria-modal="modal ? 'true' : undefined"
      :aria-labelledby="titleId"
      :aria-describedby="description ? descriptionId : undefined"
      :aria-busy="busy || undefined"
      tabindex="-1"
      @keydown="handleDrawerKeydown"
    >
      <header class="workspace-drawer-header">
        <div class="workspace-drawer-heading">
          <h2 :id="titleId">{{ title }}</h2>
          <p v-if="description" :id="descriptionId">{{ description }}</p>
        </div>
        <button
          class="icon-button subtle workspace-drawer-close"
          type="button"
          :disabled="busy"
          :title="closeLabel"
          :aria-label="closeLabel"
          @click="requestClose"
        >
          <X :size="18" />
        </button>
      </header>

      <div class="workspace-drawer-body">
        <slot />
      </div>

      <footer v-if="$slots.footer" class="workspace-drawer-footer">
        <slot name="footer" />
      </footer>
    </section>
  </div>
</template>
