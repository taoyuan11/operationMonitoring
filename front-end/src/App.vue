<script setup lang="ts">
import AdminPanel from './components/AdminPanel.vue'
import EditInstanceModal from './components/EditInstanceModal.vue'
import InstanceBoard from './components/InstanceBoard.vue'
import SummaryBand from './components/SummaryBand.vue'
import TerminalModal from './components/TerminalModal.vue'
import TopBar from './components/TopBar.vue'
import { useMonitoringConsole } from './composables/useMonitoringConsole'

const consoleState = useMonitoringConsole()
</script>

<template>
  <main class="shell" :style="consoleState.appearanceStyle.value">
    <TopBar
      :is-admin="consoleState.isAdmin.value"
      :username="consoleState.username.value"
      :current-time="consoleState.currentTime.value"
      :total="consoleState.instances.value.length"
      :online="consoleState.onlineCount.value"
      :total-traffic="consoleState.totalTraffic.value"
      :network-rx-rate="consoleState.networkRxRate.value"
      :network-tx-rate="consoleState.networkTxRate.value"
      @refresh="consoleState.refreshAll"
      @logout="consoleState.logout"
    />

    <SummaryBand
      :total="consoleState.instances.value.length"
      :online="consoleState.onlineCount.value"
      :avg-cpu="consoleState.avgCpu.value"
      :avg-memory="consoleState.avgMemory.value"
    />

    <p v-if="consoleState.errorMessage.value" class="notice">
      {{ consoleState.errorMessage.value }}
    </p>

    <section class="workspace">
      <InstanceBoard
        :instances="consoleState.instances.value"
        :commands="consoleState.commands.value"
        :is-admin="consoleState.isAdmin.value"
        :view-mode="consoleState.viewMode.value"
        @update:view-mode="consoleState.viewMode.value = $event"
        @edit="consoleState.openEdit"
        @terminal="consoleState.openTerminal"
        @disable="consoleState.disableInstance"
        @delete="consoleState.deleteInstance"
        @run-command="consoleState.runCommand"
      />

      <AdminPanel
        :is-admin="consoleState.isAdmin.value"
        :loading="consoleState.loading.value"
        :admin-tab="consoleState.adminTab.value"
        :pending-instances="consoleState.pendingInstances.value"
        :commands="consoleState.commands.value"
        :jobs="consoleState.jobs.value"
        :logs="consoleState.logs.value"
        :login-form="consoleState.loginForm"
        :settings-form="consoleState.settingsForm"
        :background-file-name="consoleState.backgroundFileName.value"
        :command-form="consoleState.commandForm"
        @update:admin-tab="consoleState.adminTab.value = $event"
        @login="consoleState.login"
        @approve="consoleState.approveInstance"
        @reject="consoleState.rejectInstance"
        @create-command="consoleState.createCommand"
        @remove-command="consoleState.removeCommand"
        @save-settings="consoleState.saveSettings"
        @select-background-image="consoleState.selectBackgroundImage"
        @clear-background-image="consoleState.clearBackgroundImage"
      />
    </section>

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
  </main>
</template>
