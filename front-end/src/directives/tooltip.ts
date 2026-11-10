import type { ObjectDirective } from 'vue'

interface TooltipState {
  text: string
  timer?: ReturnType<typeof setTimeout>
  bubble?: HTMLElement
  cleanup: () => void
  hide: () => void
}
const states = new WeakMap<HTMLElement, TooltipState>()
let active: TooltipState | undefined
let sequence = 0

export const tooltip: ObjectDirective<HTMLElement, string | undefined | null> = {
  mounted(element, binding) {
    const state: TooltipState = { text: binding.value ?? '', cleanup: () => {}, hide: () => {} }
    const hide = () => {
      clearTimeout(state.timer)
      if (state.bubble) {
        const described = element.getAttribute('aria-describedby')?.split(' ').filter((id) => id !== state.bubble?.id).join(' ')
        if (described) element.setAttribute('aria-describedby', described)
        else element.removeAttribute('aria-describedby')
        state.bubble.remove()
        state.bubble = undefined
      }
      if (active === state) active = undefined
    }
    const show = () => {
      active?.hide()
      if (!state.text) return
      active = state
      state.timer = setTimeout(() => {
        const bubble = document.createElement('div')
        bubble.className = 'ui-tooltip'
        bubble.id = `ui-tooltip-${sequence++}`
        bubble.role = 'tooltip'
        bubble.textContent = state.text
        const parent = document.fullscreenElement || document.body
        parent.append(bubble)
        state.bubble = bubble
        const rect = element.getBoundingClientRect()
        const size = bubble.getBoundingClientRect()
        bubble.style.left = `${Math.max(8, Math.min(rect.left + (rect.width - size.width) / 2, window.innerWidth - size.width - 8))}px`
        bubble.style.top = `${rect.top > size.height + 14 ? rect.top - size.height - 7 : rect.bottom + 7}px`
        element.setAttribute('aria-describedby', [element.getAttribute('aria-describedby'), bubble.id].filter(Boolean).join(' '))
      }, 400)
    }
    const pointer = (event: PointerEvent) => { if (event.pointerType !== 'touch') show() }
    const escape = (event: KeyboardEvent) => { if (event.key === 'Escape') hide() }
    state.hide = hide
    element.addEventListener('pointerenter', pointer)
    element.addEventListener('pointerleave', hide)
    element.addEventListener('focus', show)
    element.addEventListener('blur', hide)
    element.addEventListener('pointerdown', hide)
    document.addEventListener('keydown', escape)
    window.addEventListener('scroll', hide, true)
    state.cleanup = () => {
      hide()
      element.removeEventListener('pointerenter', pointer)
      element.removeEventListener('pointerleave', hide)
      element.removeEventListener('focus', show)
      element.removeEventListener('blur', hide)
      element.removeEventListener('pointerdown', hide)
      document.removeEventListener('keydown', escape)
      window.removeEventListener('scroll', hide, true)
    }
    if (element.tagName === 'BUTTON' && !element.textContent?.trim() && !element.hasAttribute('aria-label')) element.setAttribute('aria-label', state.text)
    states.set(element, state)
  },
  updated(element, binding) {
    const state = states.get(element)
    if (state && state.text !== binding.value) {
      if (element.getAttribute('aria-label') === state.text) element.setAttribute('aria-label', binding.value ?? '')
      state.text = binding.value ?? ''
      state.hide()
    }
  },
  beforeUnmount(element) { states.get(element)?.cleanup(); states.delete(element) },
}
