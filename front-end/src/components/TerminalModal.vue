<script setup lang="ts">
import { Play, X } from 'lucide-vue-next'
import type { Instance } from '../types/domain'

defineProps<{
  state: {
    instance: Instance | null
    output: string
    input: string
    connected: boolean
  }
}>()

defineEmits<{
  close: []
  send: []
}>()
</script>

<template>
  <div class="modal-backdrop" @click.self="$emit('close')">
    <section class="modal terminal-modal">
      <div class="terminal-head">
        <div>
          <h2>{{ state.instance?.name }} · Web 终端</h2>
          <p>{{ state.connected ? '已连接' : '未连接' }}</p>
        </div>
        <button class="icon-button" type="button" title="关闭" @click="$emit('close')">
          <X :size="17" />
        </button>
      </div>
      <pre class="terminal-output">{{ state.output }}</pre>
      <form class="terminal-input" @submit.prevent="$emit('send')">
        <input v-model="state.input" placeholder="输入命令" />
        <button class="primary-button" type="submit"><Play :size="16" />执行</button>
      </form>
    </section>
  </div>
</template>
