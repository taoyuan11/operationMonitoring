<script setup lang="ts">
import { ref } from 'vue'
import { CircleAlert, X } from 'lucide-vue-next'

const props = withDefaults(defineProps<{
  title: string
  message: string
  confirmLabel?: string
  tone?: 'warning' | 'danger'
  confirmationText?: string
}>(), {
  confirmLabel: '确认',
  tone: 'warning',
  confirmationText: '',
})

defineEmits<{
  close: []
  confirm: []
}>()

const enteredConfirmation = ref('')
</script>

<template>
  <div class="modal-backdrop">
    <section class="modal confirm-modal" role="alertdialog" aria-modal="true" aria-labelledby="confirm-title" aria-describedby="confirm-message">
      <button class="icon-button subtle modal-close" type="button" title="关闭" aria-label="关闭确认弹窗" @click="$emit('close')">
        <X :size="17" />
      </button>
      <span :class="['confirm-icon', tone]"><CircleAlert :size="24" /></span>
      <div class="confirm-copy">
        <h2 id="confirm-title">{{ title }}</h2>
        <p id="confirm-message">{{ message }}</p>
      </div>
      <label v-if="confirmationText" class="confirm-target-field">
        <span>输入 <strong>{{ confirmationText }}</strong> 以确认目标</span>
        <input
          v-model="enteredConfirmation"
          type="text"
          autocomplete="off"
          spellcheck="false"
          autofocus
        />
      </label>
      <form class="modal-actions" @submit.prevent="$emit('confirm')">
        <button class="text-button" type="button" :autofocus="!confirmationText" @click="$emit('close')">取消</button>
        <button
          :class="['confirm-button', tone]"
          type="submit"
          :disabled="Boolean(props.confirmationText) && enteredConfirmation !== props.confirmationText"
        >{{ confirmLabel }}</button>
      </form>
    </section>
  </div>
</template>
