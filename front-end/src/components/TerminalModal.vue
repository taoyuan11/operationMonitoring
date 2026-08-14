<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import {
  LoaderCircle,
  Plus,
  RefreshCw,
  Server,
  TerminalSquare,
  X,
} from '@lucide/vue'
import ConfirmModal from './ConfirmModal.vue'
import TerminalSession from './TerminalSession.vue'
import {
  activeTerminalSessionCount,
  listTerminalShells,
  validCustomShellProgram,
} from '../api/terminal'
import type {
  Instance,
  TerminalSessionStatus,
  TerminalShellInfo,
} from '../types/domain'

const props = defineProps<{
  instance: Instance
  instances: Instance[]
}>()

const emit = defineEmits<{
  close: []
}>()

type ShellMode = 'auto' | 'detected' | 'custom'

type WorkspaceSession = {
  id: string
  instanceId: string
  shellLabel: string
  shellProgram: string | null
  title: string
  status: TerminalSessionStatus
}

const sessions = ref<WorkspaceSession[]>([])
const activeSessionId = ref<string | null>(null)
const creatorOpen = ref(true)
const selectedInstanceId = ref(props.instance.id)
const shellMode = ref<ShellMode>('auto')
const detectedProgram = ref('')
const customProgram = ref('')
const shellOptions = ref<TerminalShellInfo[]>([])
const shellsLoading = ref(false)
const shellsError = ref('')
const maxSessions = ref(8)
const closeConfirmationOpen = ref(false)
const returnSessionId = ref<string | null>(null)
let sessionSequence = 0
let shellRequestSequence = 0

const onlineInstances = computed(() => props.instances.filter((instance) => instance.online))
const selectedInstance = computed(() =>
  props.instances.find((instance) => instance.id === selectedInstanceId.value) || null,
)
const activeSession = computed(() =>
  sessions.value.find((session) => session.id === activeSessionId.value) || null,
)
const supportsShellSelection = computed(() =>
  selectedInstance.value?.capabilities?.includes('terminal_shells_v1') === true,
)
const selectedInstanceSessionCount = computed(() =>
  activeTerminalSessionCount(sessions.value, selectedInstanceId.value),
)
const activeSessionCount = computed(() =>
  sessions.value.filter((session) => isActiveStatus(session.status)).length,
)
const customProgramValid = computed(() =>
  validCustomShellProgram(customProgram.value, selectedInstance.value?.os),
)
const createDisabled = computed(() => {
  if (!selectedInstance.value?.online) return true
  if (selectedInstanceSessionCount.value >= maxSessions.value) return true
  if (shellMode.value === 'detected') return !detectedProgram.value
  if (shellMode.value === 'custom') return !supportsShellSelection.value || !customProgramValid.value
  return false
})

watch(
  selectedInstanceId,
  () => {
    resetShellSelection()
    void loadShellOptions()
  },
  { immediate: true },
)

function resetShellSelection() {
  shellMode.value = 'auto'
  detectedProgram.value = ''
  customProgram.value = ''
  shellOptions.value = []
  shellsError.value = ''
  maxSessions.value = 8
}

async function loadShellOptions() {
  const request = ++shellRequestSequence
  const instance = selectedInstance.value
  if (!instance?.online || !supportsShellSelection.value) {
    shellsLoading.value = false
    return
  }
  shellsLoading.value = true
  shellsError.value = ''
  try {
    const response = await listTerminalShells(instance.id)
    if (request !== shellRequestSequence || selectedInstanceId.value !== instance.id) return
    shellOptions.value = response.shells
    maxSessions.value = response.max_sessions
    detectedProgram.value = response.shells[0]?.program || ''
  } catch (error) {
    if (request !== shellRequestSequence || selectedInstanceId.value !== instance.id) return
    shellsError.value = error instanceof Error ? error.message : 'Shell 列表读取失败'
  } finally {
    if (request === shellRequestSequence) shellsLoading.value = false
  }
}

function createSession() {
  const instance = selectedInstance.value
  if (!instance || createDisabled.value) return
  const shellProgram = selectedShellProgram()
  const shellLabel = selectedShellLabel(shellProgram)
  const baseTitle = `${instance.name || instance.hostname} · ${shellLabel}`
  let title = baseTitle
  let titleSequence = 2
  while (sessions.value.some((session) => session.title === title)) {
    title = `${baseTitle} #${titleSequence++}`
  }
  const id = `terminal-${Date.now()}-${++sessionSequence}`
  sessions.value.push({
    id,
    instanceId: instance.id,
    shellLabel,
    shellProgram,
    title,
    status: 'opening',
  })
  activeSessionId.value = id
  returnSessionId.value = null
  creatorOpen.value = false
}

function selectedShellProgram() {
  if (shellMode.value === 'detected') return detectedProgram.value
  if (shellMode.value === 'custom') return customProgram.value
  return null
}

function selectedShellLabel(program: string | null) {
  if (!program) return '自动'
  const detected = shellOptions.value.find((shell) => shell.program === program)
  if (detected) return detected.label
  const parts = program.split(/[\\/]/u).filter(Boolean)
  return parts.at(-1) || program
}

function openCreator() {
  const currentSession = activeSession.value
  if (!creatorOpen.value) returnSessionId.value = currentSession?.id || null
  const targetInstanceId = currentSession?.instanceId || selectedInstanceId.value
  const instanceChanged = selectedInstanceId.value !== targetInstanceId
  selectedInstanceId.value = targetInstanceId
  if (!instanceChanged) {
    resetShellSelection()
    void loadShellOptions()
  }
  activeSessionId.value = null
  creatorOpen.value = true
}

function cancelCreator() {
  creatorOpen.value = false
  activeSessionId.value = sessions.value.some((session) => session.id === returnSessionId.value)
    ? returnSessionId.value
    : sessions.value.at(-1)?.id || null
  returnSessionId.value = null
}

function activateSession(sessionId: string) {
  creatorOpen.value = false
  activeSessionId.value = sessionId
}

function updateSessionStatus(sessionId: string, status: TerminalSessionStatus) {
  const session = sessions.value.find((current) => current.id === sessionId)
  if (session) session.status = status
}

function closeSession(sessionId: string) {
  const index = sessions.value.findIndex((session) => session.id === sessionId)
  if (index < 0) return
  const wasActive = activeSessionId.value === sessionId
  sessions.value.splice(index, 1)
  if (!wasActive) return
  const next = sessions.value[index] || sessions.value[index - 1]
  activeSessionId.value = next?.id || null
  creatorOpen.value = sessions.value.length === 0
}

function requestWorkspaceClose() {
  if (activeSessionCount.value > 0) {
    closeConfirmationOpen.value = true
    return
  }
  emit('close')
}

function confirmWorkspaceClose() {
  closeConfirmationOpen.value = false
  emit('close')
}

function isActiveStatus(status: TerminalSessionStatus) {
  return status === 'opening' || status === 'ready'
}

function statusText(status: TerminalSessionStatus) {
  return {
    opening: '正在连接',
    ready: '已连接',
    closed: '已结束',
    error: '连接失败',
    disconnected: '已断开',
  }[status]
}
</script>

<template>
  <div class="modal-backdrop terminal-workspace-backdrop">
    <section class="modal terminal-modal" role="dialog" aria-modal="true" aria-labelledby="terminal-title">
      <header class="terminal-head">
        <div class="terminal-workspace-title">
          <span><TerminalSquare :size="18" /></span>
          <div>
            <h2 id="terminal-title">终端工作区</h2>
            <p>{{ sessions.length }} 个会话 · {{ activeSessionCount }} 个活动</p>
          </div>
        </div>
        <div class="terminal-head-actions">
          <button class="icon-button" type="button" title="新建终端" aria-label="新建终端" @click="openCreator">
            <Plus :size="17" />
          </button>
          <button class="icon-button" type="button" title="关闭工作区" aria-label="关闭终端工作区" @click="requestWorkspaceClose">
            <X :size="17" />
          </button>
        </div>
      </header>

      <div class="terminal-tabs" role="tablist" aria-label="终端会话">
        <div
          v-for="session in sessions"
          :key="session.id"
          :class="['terminal-tab', { active: session.id === activeSessionId && !creatorOpen }]"
          role="tab"
          tabindex="0"
          :aria-selected="session.id === activeSessionId && !creatorOpen"
          :title="session.title"
          @click="activateSession(session.id)"
          @keydown.enter="activateSession(session.id)"
          @keydown.space.prevent="activateSession(session.id)"
        >
          <span :class="['terminal-tab-status', session.status]"></span>
          <span class="terminal-tab-label">{{ session.title }}</span>
          <span class="terminal-tab-state">{{ statusText(session.status) }}</span>
          <button
            class="terminal-tab-close"
            type="button"
            title="关闭会话"
            :aria-label="`关闭 ${session.title}`"
            @click.stop="closeSession(session.id)"
            @keydown.enter.stop="closeSession(session.id)"
            @keydown.space.prevent.stop="closeSession(session.id)"
          ><X :size="13" /></button>
        </div>
        <button
          :class="['terminal-new-tab', { active: creatorOpen }]"
          type="button"
          role="tab"
          :aria-selected="creatorOpen"
          title="新建终端"
          @click="openCreator"
        ><Plus :size="15" /></button>
      </div>

      <div v-if="creatorOpen" class="terminal-creator">
        <div class="terminal-creator-heading">
          <span><Server :size="18" /></span>
          <div><h3>新建终端</h3><p>选择目标实例与 Shell</p></div>
        </div>

        <div class="terminal-creator-fields">
          <label>
            <span>实例</span>
            <select v-model="selectedInstanceId">
              <option v-for="item in onlineInstances" :key="item.id" :value="item.id">
                {{ item.name || item.hostname }} · {{ item.os }}
              </option>
            </select>
          </label>

          <div class="terminal-shell-field">
            <span class="terminal-field-label">Shell</span>
            <div class="segmented terminal-shell-modes">
              <button type="button" :class="{ active: shellMode === 'auto' }" @click="shellMode = 'auto'">自动</button>
              <button
                v-if="supportsShellSelection"
                type="button"
                :class="{ active: shellMode === 'detected' }"
                @click="shellMode = 'detected'"
              >已检测</button>
              <button
                v-if="supportsShellSelection"
                type="button"
                :class="{ active: shellMode === 'custom' }"
                @click="shellMode = 'custom'"
              >自定义</button>
            </div>
          </div>

          <label v-if="shellMode === 'detected'" class="terminal-shell-input">
            <span>可用 Shell</span>
            <select v-model="detectedProgram" :disabled="shellsLoading || shellOptions.length === 0">
              <option v-for="shell in shellOptions" :key="shell.program" :value="shell.program">
                {{ shell.label }} · {{ shell.program }}
              </option>
            </select>
          </label>

          <label v-else-if="shellMode === 'custom'" class="terminal-shell-input">
            <span>可执行文件</span>
            <input
              v-model="customProgram"
              type="text"
              autocomplete="off"
              spellcheck="false"
              placeholder="fish 或 /opt/shells/fish"
              :class="{ invalid: customProgram && !customProgramValid }"
            />
          </label>
        </div>

        <div v-if="!supportsShellSelection" class="terminal-creator-notice">
          当前 Agent 版本仅支持自动 Shell，请更新 Agent 以使用检测和自定义选择。
        </div>
        <div v-else-if="shellsLoading" class="terminal-creator-notice">
          <LoaderCircle class="spin" :size="14" />正在读取 Shell 列表
        </div>
        <div v-else-if="shellsError" class="terminal-creator-notice error" role="alert">
          <span>{{ shellsError }}</span>
          <button class="icon-button subtle" type="button" title="重试" aria-label="重新读取 Shell 列表" @click="loadShellOptions">
            <RefreshCw :size="14" />
          </button>
        </div>
        <div v-else-if="selectedInstanceSessionCount >= maxSessions" class="terminal-creator-notice error">
          该实例已达到 {{ maxSessions }} 个活动终端的上限。
        </div>

        <div class="terminal-creator-actions">
          <button v-if="sessions.length" class="text-button" type="button" @click="cancelCreator">取消</button>
          <button class="primary-button" type="button" :disabled="createDisabled" @click="createSession">
            <TerminalSquare :size="16" />创建终端
          </button>
        </div>
      </div>

      <div v-else class="terminal-session-stack">
        <TerminalSession
          v-for="session in sessions"
          v-show="session.id === activeSessionId"
          :key="session.id"
          :instance-id="session.instanceId"
          :shell-program="session.shellProgram"
          :active="session.id === activeSessionId"
          @status="updateSessionStatus(session.id, $event)"
        />
      </div>
    </section>

    <ConfirmModal
      v-if="closeConfirmationOpen"
      title="关闭终端工作区"
      :message="`将结束 ${activeSessionCount} 个正在运行的终端会话。`"
      confirm-label="结束全部会话"
      tone="danger"
      @close="closeConfirmationOpen = false"
      @confirm="confirmWorkspaceClose"
    />
  </div>
</template>
