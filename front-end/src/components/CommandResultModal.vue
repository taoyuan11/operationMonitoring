<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import {
  CircleCheck,
  CircleX,
  Clock3,
  LoaderCircle,
  Terminal,
  X,
} from 'lucide-vue-next'
import type { CommandExecutionState } from '../types/domain'
import { formatTime } from '../utils/format'

const props = defineProps<{
  execution: CommandExecutionState
}>()

const outputElement = ref<HTMLElement | null>(null)

defineEmits<{
  close: []
}>()

const terminal = computed(() =>
  props.execution.job.status === 'completed' || props.execution.job.status === 'failed',
)

const statusLabel = computed(() => {
  switch (props.execution.job.status) {
    case 'queued': return '等待下发'
    case 'running': return '正在执行'
    case 'completed': return '执行成功'
    case 'failed': return '执行失败'
    default: return props.execution.job.status
  }
})

const outputText = computed(() => {
  if (props.execution.job.output) return props.execution.job.output
  if (terminal.value) return '命令执行完成，未产生输出。'
  return '等待实例返回执行结果...'
})

watch(
  () => props.execution.job.output,
  async () => {
    await nextTick()
    if (!outputElement.value || terminal.value) return
    outputElement.value.scrollTop = outputElement.value.scrollHeight
  },
)
</script>

<template>
  <div class="modal-backdrop">
    <section
      class="modal command-result-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="command-result-title"
    >
      <header class="modal-header">
        <div class="modal-title">
          <span><Terminal :size="18" /></span>
          <div>
            <h2 id="command-result-title">快捷命令输出</h2>
            <p>{{ execution.instanceName }} · {{ execution.commandName }}</p>
          </div>
        </div>
        <button
          class="icon-button subtle"
          type="button"
          title="关闭"
          aria-label="关闭命令执行结果"
          @click="$emit('close')"
        >
          <X :size="17" />
        </button>
      </header>

      <div :class="['command-result-status', execution.job.status]" role="status">
        <LoaderCircle v-if="!terminal" class="spin" :size="18" />
        <CircleCheck v-else-if="execution.job.status === 'completed'" :size="18" />
        <CircleX v-else :size="18" />
        <div>
          <strong>{{ statusLabel }}</strong>
          <small v-if="execution.job.exit_code !== null">退出码 {{ execution.job.exit_code }}</small>
          <small v-else>等待 Agent 返回</small>
        </div>
        <time><Clock3 :size="13" />{{ formatTime(execution.job.completed_at || execution.job.created_at) }}</time>
      </div>

      <p v-if="execution.error" class="command-result-error">{{ execution.error }}</p>

      <div class="command-result-command">
        <span>执行命令</span>
        <code>{{ execution.job.command }}</code>
      </div>

      <div class="command-result-output">
        <span>输出</span>
        <pre ref="outputElement" aria-live="polite">{{ outputText }}</pre>
      </div>

      <footer class="modal-actions">
        <button class="text-button" type="button" @click="$emit('close')">关闭</button>
      </footer>
    </section>
  </div>
</template>
