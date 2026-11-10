<script setup lang="ts">
import UiButton from './ui/UiButton.vue'
import UiForm from './ui/UiForm.vue'
import UiInput from './ui/UiInput.vue'
import { ref } from 'vue'
import { CircleAlert, X } from '@lucide/vue'

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
      <UiButton class="icon-button subtle modal-close" type="button" v-tooltip="'关闭'" aria-label="关闭确认弹窗" @click="$emit('close')">
        <X :size="17" />
      </UiButton>
      <span :class="['confirm-icon', tone]"><CircleAlert :size="24" /></span>
      <div class="confirm-copy">
        <h2 id="confirm-title">{{ title }}</h2>
        <p id="confirm-message">{{ message }}</p>
      </div>
      <label v-if="confirmationText" class="confirm-target-field">
        <span>输入 <strong>{{ confirmationText }}</strong> 以确认目标</span>
        <UiInput
          v-model="enteredConfirmation"
          type="text"
          autocomplete="off"
          spellcheck="false"
          autofocus
        />
      </label>
      <UiForm class="modal-actions" @submit.prevent="$emit('confirm')">
        <UiButton class="text-button" type="button" :autofocus="!confirmationText" @click="$emit('close')">取消</UiButton>
        <UiButton
          :class="['confirm-button', tone]"
          type="submit"
          :disabled="Boolean(props.confirmationText) && enteredConfirmation !== props.confirmationText"
        >{{ confirmLabel }}</UiButton>
      </UiForm>
    </section>
  </div>
</template>
