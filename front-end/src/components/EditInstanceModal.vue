<script setup lang="ts">
defineProps<{
  form: {
    name: string
    region: string
    remark: string
  }
}>()

defineEmits<{
  close: []
  save: []
}>()
</script>

<template>
  <div class="modal-backdrop" @click.self="$emit('close')">
    <form class="modal" @submit.prevent="$emit('save')">
      <h2>编辑实例</h2>
      <label>
        实例名称
        <input v-model="form.name" />
      </label>
      <label>
        实例地区
        <input v-model="form.region" />
      </label>
      <label>
        实例备注
        <textarea v-model="form.remark"></textarea>
      </label>
      <div class="modal-actions">
        <button class="text-button" type="button" @click="$emit('close')">取消</button>
        <button class="primary-button" type="submit">保存</button>
      </div>
    </form>
  </div>
</template>
