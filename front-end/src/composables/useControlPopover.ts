import { nextTick, onBeforeUnmount, ref, watch, type Ref } from 'vue'

export function useControlPopover(trigger: Ref<HTMLElement | null>, panel: Ref<HTMLElement | null>, width?: number) {
  const open = ref(false)
  const style = ref<Record<string, string>>({})
  const target = ref<Element | string>('body')
  let observer: ResizeObserver | undefined

  function position() {
    if (!open.value || !trigger.value || !panel.value) return
    const rect = trigger.value.getBoundingClientRect()
    const viewport = window.visualViewport
    const viewportWidth = viewport?.width ?? window.innerWidth
    const viewportHeight = viewport?.height ?? window.innerHeight
    const top = viewport?.offsetTop ?? 0
    const left = viewport?.offsetLeft ?? 0
    const panelWidth = Math.min(width ?? Math.max(rect.width, 180), viewportWidth - 24)
    const below = viewportHeight + top - rect.bottom - 18
    const above = rect.top - top - 18
    const up = below < panel.value.scrollHeight && above > below
    const maxHeight = Math.max(80, up ? above : below)
    style.value = {
      width: `${panelWidth}px`,
      left: `${Math.max(left + 12, Math.min(rect.left, left + viewportWidth - panelWidth - 12))}px`,
      top: `${up ? Math.max(top + 12, rect.top - Math.min(panel.value.scrollHeight, maxHeight) - 6) : rect.bottom + 6}px`,
      maxHeight: `${maxHeight}px`,
    }
  }

  function close(restoreFocus = false) {
    open.value = false
    if (restoreFocus) trigger.value?.focus({ preventScroll: true })
  }

  function outside(event: Event) {
    const node = event.target as Node
    if (!trigger.value?.contains(node) && !panel.value?.contains(node)) close()
  }

  function keydown(event: KeyboardEvent) {
    if (event.key === 'Escape' && !event.isComposing) {
      event.preventDefault()
      event.stopImmediatePropagation()
      close(true)
    }
  }

  function cleanup() {
    observer?.disconnect()
    observer = undefined
    document.removeEventListener('pointerdown', outside, true)
    document.removeEventListener('focusin', outside)
    document.removeEventListener('keydown', keydown, true)
    window.removeEventListener('resize', position)
    window.removeEventListener('scroll', position, true)
    window.visualViewport?.removeEventListener('resize', position)
  }

  watch(open, async (value) => {
    cleanup()
    if (!value) return
    target.value = document.fullscreenElement || 'body'
    await nextTick()
    if (!open.value) return
    position()
    observer = new ResizeObserver(position)
    if (panel.value) observer.observe(panel.value)
    if (trigger.value) observer.observe(trigger.value)
    document.addEventListener('pointerdown', outside, true)
    document.addEventListener('focusin', outside)
    document.addEventListener('keydown', keydown, true)
    window.addEventListener('resize', position)
    window.addEventListener('scroll', position, true)
    window.visualViewport?.addEventListener('resize', position)
  })
  onBeforeUnmount(cleanup)
  return { open, style, target, close, position }
}
