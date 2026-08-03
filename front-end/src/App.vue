<script setup lang="ts">
import { computed, defineAsyncComponent, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import AdminNavigation from './components/AdminNavigation.vue'
import AdminPanel from './components/AdminPanel.vue'
import AgentUpdatesPanel from './components/AgentUpdatesPanel.vue'
import ConfirmModal from './components/ConfirmModal.vue'
import InstanceBoard from './components/InstanceBoard.vue'
import LoginModal from './components/LoginModal.vue'
import SummaryBand from './components/SummaryBand.vue'
import TopBar from './components/TopBar.vue'
import UserManagementPanel from './components/UserManagementPanel.vue'
import { useMonitoringConsole } from './composables/useMonitoringConsole'
import type {
  AdminTab,
  AdminUser,
  AgentRelease,
  AppPage,
  AuthenticatorDevice,
  CommandRecord,
  Instance,
} from './types/domain'

const TerminalModal = defineAsyncComponent(() => import('./components/TerminalModal.vue'))
const RemoteDesktopModal = defineAsyncComponent(() => import('./components/RemoteDesktopModal.vue'))
const EditInstanceModal = defineAsyncComponent(() => import('./components/EditInstanceModal.vue'))
const InstanceDetailModal = defineAsyncComponent(() => import('./components/InstanceDetailModal.vue'))
const CommandResultModal = defineAsyncComponent(() => import('./components/CommandResultModal.vue'))

const consoleState = useMonitoringConsole()
const currentPage = ref<AppPage>('home')
const loginOpen = ref(false)
const selectedInstanceId = ref('')
const confirmation = ref<{
  title: string
  message: string
  confirmLabel: string
  tone: 'warning' | 'danger'
  confirmationText?: string
  action: () => void
} | null>(null)

const activeAdminTab = computed<AdminTab>(() =>
  currentPage.value === 'home' ? consoleState.adminTab.value : currentPage.value,
)

const selectedInstance = computed(() =>
  consoleState.instances.value.find((instance) => instance.id === selectedInstanceId.value) || null,
)

const pageFromHash: Record<string, AppPage> = {
  '#/': 'home',
  '#/instances': 'home',
  '#/approval': 'pending',
  '#/commands': 'commands',
  '#/updates': 'updates',
  '#/users': 'users',
  '#/logs': 'logs',
  '#/settings': 'settings',
}

const hashFromPage: Record<AppPage, string> = {
  home: '#/',
  pending: '#/approval',
  commands: '#/commands',
  updates: '#/updates',
  users: '#/users',
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
    selectedInstanceId.value = ''
    confirmation.value = null
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

async function logout() {
  const success = await consoleState.logout()
  if (!success) return
  selectedInstanceId.value = ''
  navigate('home')
}

function requestDisable(instance: Instance) {
  selectedInstanceId.value = ''
  confirmation.value = {
    title: '停用节点',
    message: `停用 ${instance.name || instance.hostname} 后将不再接受该节点上报。`,
    confirmLabel: '确认停用',
    tone: 'warning',
    action: () => consoleState.disableInstance(instance),
  }
}

function requestDelete(instance: Instance) {
  selectedInstanceId.value = ''
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
    message: `停用“${command.name}”后，实例操作面板将不再提供此命令。`,
    confirmLabel: '确认停用',
    tone: 'warning',
    action: () => consoleState.removeCommand(command),
  }
}

function requestRunCommand(instance: Instance, command: CommandRecord) {
  selectedInstanceId.value = ''
  confirmation.value = {
    title: '执行快捷命令',
    message: command.confirm_text || `将在 ${instance.name || instance.hostname} 上执行：${command.command}`,
    confirmLabel: '确认执行',
    tone: 'warning',
    action: () => consoleState.runCommand(instance, command),
  }
}

function openInstance(instance: Instance) {
  selectedInstanceId.value = instance.id
}

function editSelectedInstance(instance: Instance) {
  selectedInstanceId.value = ''
  consoleState.openEdit(instance)
}

function openSelectedTerminal(instance: Instance) {
  selectedInstanceId.value = ''
  consoleState.openTerminal(instance)
}

function openSelectedRemoteDesktop(instance: Instance) {
  selectedInstanceId.value = ''
  consoleState.openRemoteDesktop(instance)
}

function requestPublishAgentRelease(release: AgentRelease) {
  const draftArtifacts = release.artifacts.filter((artifact) => artifact.status === 'draft')
  const targets = draftArtifacts.map((artifact) => `${artifact.os}/${artifact.native_arch}`).join('、')
  const isAdditionalBatch = release.status === 'published'
  confirmation.value = {
    title: isAdditionalBatch ? '发布新增更新包' : '发布 Agent 更新',
    message: `将发布 ${release.version} 的 ${draftArtifacts.length} 个更新包（${targets}）。符合条件的实例会自动安装对应更新包。尚未完成过受管更新的实例可能没有可用的回滚包。`,
    confirmLabel: isAdditionalBatch ? '确认发布新增包' : '确认发布',
    tone: 'warning',
    action: () => consoleState.publishAgentRelease(release),
  }
}

function requestDeleteAgentRelease(release: AgentRelease) {
  if (release.status === 'published') {
    confirmation.value = {
      title: '永久删除已发布版本',
      message: `将永久删除 Agent ${release.version}、${release.artifacts.length} 个已上传程序文件及其 SHA-256 校验文件，以及 ${release.attempts.length} 条实例更新记录。已安装此版本的实例不会回退，此操作无法恢复。`,
      confirmLabel: '永久删除版本',
      tone: 'danger',
      confirmationText: release.version,
      action: () => consoleState.deleteAgentRelease(release),
    }
    return
  }
  confirmation.value = {
    title: '删除更新草稿',
    message: `将删除 ${release.version} 及其已上传的可执行文件，此操作无法恢复。`,
    confirmLabel: '删除草稿',
    tone: 'danger',
    action: () => consoleState.deleteAgentRelease(release),
  }
}

function requestCancelAuthEnrollment(enrollmentId: string) {
  const enrollment = consoleState.activeAuthEnrollment.value?.id === enrollmentId
    ? consoleState.activeAuthEnrollment.value
    : consoleState.authEnrollments.value.find((item) => item.id === enrollmentId)
  if (!enrollment) return
  const target = `${enrollment.username} · ${enrollment.device_name}`
  confirmation.value = {
    title: '取消认证设备注册',
    message: `将取消 ${target} 的待确认注册，已生成的二维码将立即失效。`,
    confirmLabel: '取消注册',
    tone: 'danger',
    confirmationText: target,
    action: () => consoleState.cancelAuthEnrollment(enrollmentId),
  }
}

function requestDeleteAdminUser(user: AdminUser) {
  confirmation.value = {
    title: '删除管理员',
    message: `将永久删除 ${user.username}，并立即撤销该用户的全部会话和认证设备。`,
    confirmLabel: '永久删除',
    tone: 'danger',
    confirmationText: user.username,
    action: () => consoleState.deleteAdminUser(user),
  }
}

function requestRevokeAuthenticatorDevice(device: AuthenticatorDevice) {
  const user = consoleState.adminUsers.value.find((item) =>
    item.devices.some((candidate) => candidate.id === device.id),
  )
  if (!user) return
  const target = `${user.username} · ${device.name}`
  confirmation.value = {
    title: '撤销认证设备',
    message: `将撤销 ${target}，通过该设备建立的会话会立即失效。`,
    confirmLabel: '确认撤销',
    tone: 'danger',
    confirmationText: target,
    action: () => consoleState.revokeAuthenticatorDevice(device.id),
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

    <Transition name="navigation">
      <AdminNavigation
        v-if="consoleState.isAdmin.value"
        :current-page="currentPage"
        :pending-count="consoleState.pendingInstances.value.length"
        @navigate="navigate"
      />
    </Transition>

    <Transition name="page" mode="out-in">
      <section :key="currentPage" :class="['page-stage', `page-stage-${currentPage}`]">
        <template v-if="currentPage === 'home'">
          <Transition name="content" mode="out-in">
            <div
              v-if="!consoleState.publicReady.value"
              key="skeleton"
              class="dashboard-skeleton"
              aria-label="正在加载监控数据"
            >
              <div class="skeleton-summary"><i v-for="index in 4" :key="index"></i></div>
              <div class="skeleton-heading"></div>
              <div class="skeleton-board"><i v-for="index in 3" :key="index"></i></div>
            </div>

            <div v-else key="dashboard" class="dashboard-content">
              <SummaryBand
                :total="consoleState.instances.value.length"
                :online="consoleState.onlineCount.value"
                :avg-cpu="consoleState.avgCpu.value"
                :avg-memory="consoleState.avgMemory.value"
              />

              <Transition name="notice">
                <p v-if="consoleState.errorMessage.value" class="notice">
                  {{ consoleState.errorMessage.value }}
                </p>
              </Transition>

              <InstanceBoard
                :instances="consoleState.instances.value"
                :is-admin="consoleState.isAdmin.value"
                :view-mode="consoleState.viewMode.value"
                @update:view-mode="consoleState.viewMode.value = $event"
                @open="openInstance"
              />
            </div>
          </Transition>
        </template>

        <template v-else>
          <Transition name="notice">
            <p v-if="consoleState.errorMessage.value" class="notice page-notice">
              {{ consoleState.errorMessage.value }}
            </p>
          </Transition>

          <AgentUpdatesPanel
            v-if="currentPage === 'updates'"
            :key="`updates-${consoleState.adminResetKey.value}`"
            :instances="consoleState.instances.value"
            :releases="consoleState.agentReleases.value"
            :attempts="consoleState.agentUpdateAttempts.value"
            :form="consoleState.agentReleaseForm"
            :operation="consoleState.agentUpdateOperation.value"
            :busy-id="consoleState.agentUpdateBusyId.value"
            :message="consoleState.agentUpdateMessage.value"
            @create-release="consoleState.createAgentRelease"
            @save-release="consoleState.saveAgentRelease"
            @upload-artifact="consoleState.uploadAgentArtifact"
            @delete-artifact="consoleState.deleteAgentArtifact"
            @publish-release="requestPublishAgentRelease"
            @delete-release="requestDeleteAgentRelease"
            @retry-attempt="consoleState.retryAgentUpdateAttempt"
          />
          <UserManagementPanel
            v-else-if="currentPage === 'users'"
            :key="`users-${consoleState.adminResetKey.value}`"
            :users="consoleState.adminUsers.value"
            :enrollments="consoleState.authEnrollments.value"
            :active-enrollment="consoleState.activeAuthEnrollment.value"
            :current-user="consoleState.currentUser.value"
            :loading="consoleState.loading.value"
            :form="consoleState.userAuthForm"
            @create-user="consoleState.createUserEnrollment"
            @add-device="consoleState.createDeviceEnrollment"
            @confirm-enrollment="consoleState.confirmAuthEnrollment"
            @cancel-enrollment="requestCancelAuthEnrollment"
            @set-enabled="consoleState.setAdminUserEnabled"
            @delete-user="requestDeleteAdminUser"
            @revoke-device="requestRevokeAuthenticatorDevice"
          />
          <AdminPanel
            v-else
            :key="`admin-${consoleState.adminResetKey.value}`"
            :admin-tab="activeAdminTab"
            :pending-instances="consoleState.pendingInstances.value"
            :commands="consoleState.commands.value"
            :jobs="consoleState.jobs.value"
            :logs="consoleState.logs.value"
            :settings-form="consoleState.settingsForm"
            :resolved-theme="consoleState.resolvedTheme.value"
            :appearance-message="consoleState.appearanceMessage.value"
            :background-file-name="consoleState.backgroundFileName.value"
            :background-operation="consoleState.backgroundOperation.value"
            :background-message="consoleState.backgroundMessage.value"
            :command-form="consoleState.commandForm"
            @approve="consoleState.approveInstance"
            @reject="consoleState.rejectInstance"
            @create-command="consoleState.createCommand"
            @remove-command="requestRemoveCommand"
            @save-settings="consoleState.saveSettings"
            @save-appearance="consoleState.saveAppearance"
            @appearance-changed="consoleState.appearanceMessage.value = ''"
            @select-background-image="consoleState.selectBackgroundImage"
            @clear-background-image="consoleState.clearBackgroundImage"
          />
        </template>
      </section>
    </Transition>

    <Transition name="modal" appear>
      <InstanceDetailModal
        v-if="selectedInstance"
        :instance="selectedInstance"
        :is-admin="consoleState.isAdmin.value"
        :commands="consoleState.commands.value"
        :loading="consoleState.loading.value"
        @close="selectedInstanceId = ''"
        @edit="editSelectedInstance"
        @terminal="openSelectedTerminal"
        @remote-desktop="openSelectedRemoteDesktop"
        @disable="requestDisable"
        @delete="requestDelete"
        @run-command="requestRunCommand"
      />
    </Transition>

    <Transition name="modal" appear>
      <LoginModal
        v-if="loginOpen && !consoleState.isAdmin.value"
        :loading="consoleState.loading.value"
        :error-message="consoleState.errorMessage.value"
        :mode="consoleState.authMode.value"
        :enrollment="consoleState.activeAuthEnrollment.value"
        :form="consoleState.loginForm"
        @close="loginOpen = false"
        @login="consoleState.login"
        @restart="consoleState.restartBootstrap"
      />
    </Transition>

    <Transition name="modal" appear>
      <EditInstanceModal
        v-if="consoleState.editInstance.value && consoleState.isAdmin.value"
        :form="consoleState.editForm"
        @close="consoleState.closeEdit"
        @save="consoleState.saveEdit"
      />
    </Transition>

    <Transition name="modal" appear>
      <TerminalModal
        v-if="consoleState.terminalState.instance"
        :instance="consoleState.terminalState.instance"
        @close="consoleState.closeTerminal"
      />
    </Transition>

    <Transition name="modal" appear>
      <RemoteDesktopModal
        v-if="consoleState.remoteDesktopState.instance"
        :instance="consoleState.remoteDesktopState.instance"
        @close="consoleState.closeRemoteDesktop"
      />
    </Transition>

    <Transition name="modal" appear>
      <CommandResultModal
        v-if="consoleState.commandExecution.value"
        :execution="consoleState.commandExecution.value"
        @close="consoleState.closeCommandExecution"
      />
    </Transition>

    <Transition name="modal" appear>
      <ConfirmModal
        v-if="confirmation"
        :title="confirmation.title"
        :message="confirmation.message"
        :confirm-label="confirmation.confirmLabel"
        :tone="confirmation.tone"
        :confirmation-text="confirmation.confirmationText"
        @close="confirmation = null"
        @confirm="confirmAction"
      />
    </Transition>
  </main>
</template>
