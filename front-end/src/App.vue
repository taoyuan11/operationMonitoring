<script setup lang="ts">
import { computed, defineAsyncComponent, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import AdminNavigation from './components/AdminNavigation.vue'
import AdminPanel from './components/AdminPanel.vue'
import ConfirmModal from './components/ConfirmModal.vue'
import InstanceBoard from './components/InstanceBoard.vue'
import LoginModal from './components/LoginModal.vue'
import SummaryBand from './components/SummaryBand.vue'
import TopBar from './components/TopBar.vue'
import { useMonitoringConsole } from './composables/useMonitoringConsole'
import type { AdminTab, AppPage, CommandRecord, Instance } from './types/domain'

const TerminalModal = defineAsyncComponent(() => import('./components/TerminalModal.vue'))
const EditInstanceModal = defineAsyncComponent(() => import('./components/EditInstanceModal.vue'))

const consoleState = useMonitoringConsole()
const currentPage = ref<AppPage>('home')
const loginOpen = ref(false)
const confirmation = ref<{
  title: string
  message: string
  confirmLabel: string
  tone: 'warning' | 'danger'
  action: () => void
} | null>(null)

const activeAdminTab = computed<AdminTab>(() =>
  currentPage.value === 'home' ? consoleState.adminTab.value : currentPage.value,
)

const pageFromHash: Record<string, AppPage> = {
  '#/': 'home',
  '#/instances': 'home',
  '#/approval': 'pending',
  '#/commands': 'commands',
  '#/logs': 'logs',
  '#/settings': 'settings',
}

const hashFromPage: Record<AppPage, string> = {
  home: '#/',
  pending: '#/approval',
  commands: '#/commands',
  logs: '#/logs',
  settings: '#/settings',
}

watch(
  [() => consoleState.sessionReady.value, () => consoleState.isAdmin.value],
  ([ready, isAdmin]) => {
    if (!ready) return
    if (isAdmin) {
      loginOpen.value = false
      syncPageFromHash()
      return
    }
    currentPage.value = 'home'
    if (window.location.hash && window.location.hash !== '#/') {
      window.history.replaceState(null, '', '#/')
    }
  },
)

onMounted(() => {
  window.addEventListener('hashchange', syncPageFromHash)
})

onBeforeUnmount(() => {
  window.removeEventListener('hashchange', syncPageFromHash)
})

function navigate(page: AppPage) {
  if (page !== 'home' && !consoleState.isAdmin.value) {
    loginOpen.value = true
    return
  }
  currentPage.value = page
  if (page !== 'home') consoleState.adminTab.value = page
  if (window.location.hash !== hashFromPage[page]) {
    window.location.hash = hashFromPage[page]
  }
}

function syncPageFromHash() {
  const page = pageFromHash[window.location.hash] || 'home'
  if (page !== 'home' && !consoleState.isAdmin.value) {
    currentPage.value = 'home'
    return
  }
  currentPage.value = page
  if (page !== 'home') consoleState.adminTab.value = page
}

function openLogin() {
  consoleState.errorMessage.value = ''
  loginOpen.value = true
}

function logout() {
  navigate('home')
  consoleState.logout()
}

function requestDisable(instance: Instance) {
  confirmation.value = {
    title: '停用节点',
    message: `停用 ${instance.name || instance.hostname} 后将不再接受该节点上报。`,
    confirmLabel: '确认停用',
    tone: 'warning',
    action: () => consoleState.disableInstance(instance),
  }
}

function requestDelete(instance: Instance) {
  confirmation.value = {
    title: '删除节点',
    message: `将永久删除 ${instance.name || instance.hostname} 及其历史指标，此操作无法恢复。`,
    confirmLabel: '永久删除',
    tone: 'danger',
    action: () => consoleState.deleteInstance(instance),
  }
}

function requestRemoveCommand(command: CommandRecord) {
  confirmation.value = {
    title: '停用快捷命令',
    message: `停用“${command.name}”后，节点卡片将不再提供此操作。`,
    confirmLabel: '确认停用',
    tone: 'warning',
    action: () => consoleState.removeCommand(command),
  }
}

function requestRunCommand(instance: Instance, command: CommandRecord) {
  confirmation.value = {
    title: '执行快捷命令',
    message: command.confirm_text || `将在 ${instance.name || instance.hostname} 上执行：${command.command}`,
    confirmLabel: '确认执行',
    tone: 'warning',
    action: () => consoleState.runCommand(instance, command),
  }
}

function confirmAction() {
  const action = confirmation.value?.action
  confirmation.value = null
  action?.()
}
</script>

<template>
  <main
    class="shell"
    :class="{ 'has-custom-background': consoleState.backgroundImageUrl.value }"
    :style="consoleState.appearanceStyle.value"
  >
    <TopBar
      :is-admin="consoleState.isAdmin.value"
      :current-time="consoleState.currentTime.value"
      :total="consoleState.instances.value.length"
      :online="consoleState.onlineCount.value"
      :total-traffic="consoleState.totalTraffic.value"
      :network-rx-rate="consoleState.networkRxRate.value"
      :network-tx-rate="consoleState.networkTxRate.value"
      :refreshing="consoleState.refreshing.value"
      @refresh="consoleState.refreshAll"
      @login="openLogin"
      @logout="logout"
    />

    <AdminNavigation
      v-if="consoleState.isAdmin.value"
      :current-page="currentPage"
      :pending-count="consoleState.pendingInstances.value.length"
      @navigate="navigate"
    />

    <template v-if="currentPage === 'home'">
      <div v-if="!consoleState.publicReady.value" class="dashboard-skeleton" aria-label="正在加载监控数据">
        <div class="skeleton-summary"><i v-for="index in 4" :key="index"></i></div>
        <div class="skeleton-heading"></div>
        <div class="skeleton-board"><i v-for="index in 3" :key="index"></i></div>
      </div>

      <template v-else>
        <SummaryBand
          :total="consoleState.instances.value.length"
          :online="consoleState.onlineCount.value"
          :avg-cpu="consoleState.avgCpu.value"
          :avg-memory="consoleState.avgMemory.value"
        />

        <p v-if="consoleState.errorMessage.value" class="notice">
          {{ consoleState.errorMessage.value }}
        </p>

        <InstanceBoard
          :instances="consoleState.instances.value"
          :commands="consoleState.commands.value"
          :is-admin="consoleState.isAdmin.value"
          :view-mode="consoleState.viewMode.value"
          @update:view-mode="consoleState.viewMode.value = $event"
          @edit="consoleState.openEdit"
          @terminal="consoleState.openTerminal"
          @disable="requestDisable"
          @delete="requestDelete"
          @run-command="requestRunCommand"
        />
      </template>
    </template>

    <template v-else>
      <p v-if="consoleState.errorMessage.value" class="notice page-notice">
        {{ consoleState.errorMessage.value }}
      </p>

      <AdminPanel
        :admin-tab="activeAdminTab"
        :pending-instances="consoleState.pendingInstances.value"
        :commands="consoleState.commands.value"
        :jobs="consoleState.jobs.value"
        :logs="consoleState.logs.value"
        :settings-form="consoleState.settingsForm"
        :background-file-name="consoleState.backgroundFileName.value"
        :background-operation="consoleState.backgroundOperation.value"
        :background-message="consoleState.backgroundMessage.value"
        :command-form="consoleState.commandForm"
        @approve="consoleState.approveInstance"
        @reject="consoleState.rejectInstance"
        @create-command="consoleState.createCommand"
        @remove-command="requestRemoveCommand"
        @save-settings="consoleState.saveSettings"
        @select-background-image="consoleState.selectBackgroundImage"
        @clear-background-image="consoleState.clearBackgroundImage"
      />
    </template>

    <LoginModal
      v-if="loginOpen && !consoleState.isAdmin.value"
      :loading="consoleState.loading.value"
      :error-message="consoleState.errorMessage.value"
      :form="consoleState.loginForm"
      @close="loginOpen = false"
      @login="consoleState.login"
    />

    <EditInstanceModal
      v-if="consoleState.editInstance.value"
      :form="consoleState.editForm"
      @close="consoleState.closeEdit"
      @save="consoleState.saveEdit"
    />

    <TerminalModal
      v-if="consoleState.terminalState.instance"
      :instance="consoleState.terminalState.instance"
      @close="consoleState.closeTerminal"
    />

    <ConfirmModal
      v-if="confirmation"
      :title="confirmation.title"
      :message="confirmation.message"
      :confirm-label="confirmation.confirmLabel"
      :tone="confirmation.tone"
      @close="confirmation = null"
      @confirm="confirmAction"
    />
  </main>
</template>
