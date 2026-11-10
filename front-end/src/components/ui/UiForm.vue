<script setup lang="ts">
import { onBeforeUnmount, useId } from 'vue'

const id = useId()
const errors = new Map<HTMLInputElement, { message: HTMLElement; target: HTMLElement; describedBy: string | null; invalid: string | null }>()
let focusQueued = false
let sequence = 0

function clear(input: HTMLInputElement) {
  const error = errors.get(input)
  if (!error) return
  error.message.remove()
  if (error.describedBy === null) error.target.removeAttribute('aria-describedby')
  else error.target.setAttribute('aria-describedby', error.describedBy)
  if (error.invalid === null) error.target.removeAttribute('aria-invalid')
  else error.target.setAttribute('aria-invalid', error.invalid)
  const control = input.closest('.ui-control, .input-shell') || input
  control.classList.remove('is-invalid')
  errors.delete(input)
}

function invalid(event: Event) {
  event.preventDefault()
  const input = event.target as HTMLInputElement
  clear(input)
  const message = document.createElement('small')
  message.className = 'ui-field-error'
  message.id = `${id}-error-${sequence++}`
  message.setAttribute('role', 'alert')
  message.textContent = input.validationMessage || '请检查此项内容'
  const control = input.closest('.ui-control, .input-shell') || input
  const target = input.classList.contains('ui-validation-proxy') ? control.querySelector<HTMLElement>('button') || input : input
  errors.set(input, { message, target, describedBy: target.getAttribute('aria-describedby'), invalid: target.getAttribute('aria-invalid') })
  target.setAttribute('aria-invalid', 'true')
  target.setAttribute('aria-describedby', [target.getAttribute('aria-describedby'), message.id].filter(Boolean).join(' '))
  control.classList.add('is-invalid')
  control.after(message)
  if (!focusQueued) {
    focusQueued = true
    queueMicrotask(() => {
      target.focus()
      focusQueued = false
    })
  }
}

function edited(event: Event) {
  const input = event.target as HTMLInputElement
  if (input.validity?.valid) clear(input)
}

onBeforeUnmount(() => { for (const input of errors.keys()) clear(input) })
</script>

<template>
  <form @invalid.capture="invalid" @input="edited" @change="edited"><slot /></form>
</template>
