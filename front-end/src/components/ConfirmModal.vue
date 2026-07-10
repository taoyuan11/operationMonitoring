<script setup lang="ts">
import { CircleAlert, X } from 'lucide-vue-next'

withDefaults(defineProps<{
  title: string
  message: string
  confirmLabel?: string
  tone?: 'warning' | 'danger'
}>(), {
  confirmLabel: '确认',
  tone: 'warning',
})

defineEmits<{
  close: []
  confirm: []
}>()
</script>

<template>
  <div class="modal-backdrop" @click.self="$emit('close')" @keydown.esc="$emit('close')">
    <section class="modal confirm-modal" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" aria-describedby="confirm-message">
      <button class="icon-button subtle modal-close" type="button" title="关闭" aria-label="关闭确认弹窗" @click="$emit('close')">
        <X :size="17" />
      </button>
      <span :class="['confirm-icon', tone]"><CircleAlert :size="24" /></span>
      <div class="confirm-copy">
        <h2 id="confirm-title">{{ title }}</h2>
        <p id="confirm-message">{{ message }}</p>
      </div>
      <div class="modal-actions">
        <button class="text-button" type="button" autofocus @click="$emit('close')">取消</button>
        <button :class="['confirm-button', tone]" type="button" @click="$emit('confirm')">{{ confirmLabel }}</button>
      </div>
    </section>
  </div>
</template>
