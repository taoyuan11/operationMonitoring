<script setup lang="ts">
import {
  Check,
  ClipboardList,
  Image,
  ListChecks,
  LogIn,
  Plus,
  Settings,
  Terminal,
  Trash2,
  X,
} from 'lucide-vue-next'
import type { ActionLog, AdminTab, CommandJob, CommandRecord, PendingInstance } from '../types/domain'
import { formatTime } from '../utils/format'

defineProps<{
  isAdmin: boolean
  loading: boolean
  adminTab: AdminTab
  pendingInstances: PendingInstance[]
  commands: CommandRecord[]
  jobs: CommandJob[]
  logs: ActionLog[]
  loginForm: {
    username: string
    password: string
  }
  settingsForm: {
    retention_days: number
    background_image_url: string | null
  }
  backgroundFileName: string
  commandForm: {
    name: string
    command: string
    confirm_text: string
  }
}>()

defineEmits<{
  'update:adminTab': [value: AdminTab]
  login: []
  approve: [id: string]
  reject: [id: string]
  createCommand: []
  removeCommand: [command: CommandRecord]
  saveSettings: []
  selectBackgroundImage: [event: Event]
  clearBackgroundImage: []
}>()
</script>

<template>
  <aside class="admin-pane">
    <form v-if="!isAdmin" class="login-panel" @submit.prevent="$emit('login')">
      <h2>管理员登录</h2>
      <label>
        账号
        <input v-model="loginForm.username" autocomplete="username" />
      </label>
      <label>
        密码
        <input v-model="loginForm.password" type="password" autocomplete="current-password" />
      </label>
      <button class="primary-button" type="submit" :disabled="loading">
        <LogIn :size="17" />登录
      </button>
    </form>

    <div v-else class="admin-panel">
      <div class="tabs">
        <button :class="{ active: adminTab === 'pending' }" type="button" @click="$emit('update:adminTab', 'pending')">
          <ListChecks :size="16" />审批
        </button>
        <button :class="{ active: adminTab === 'commands' }" type="button" @click="$emit('update:adminTab', 'commands')">
          <Terminal :size="16" />命令
        </button>
        <button :class="{ active: adminTab === 'settings' }" type="button" @click="$emit('update:adminTab', 'settings')">
          <Settings :size="16" />设置
        </button>
        <button :class="{ active: adminTab === 'logs' }" type="button" @click="$emit('update:adminTab', 'logs')">
          <ClipboardList :size="16" />日志
        </button>
      </div>

      <section v-if="adminTab === 'pending'" class="admin-section">
        <h2>待审批实例</h2>
        <p v-if="pendingInstances.length === 0" class="muted">暂无待审批实例。</p>
        <article v-for="item in pendingInstances" :key="item.id" class="list-item">
          <div>
            <strong>{{ item.hostname }}</strong>
            <span>{{ item.os }}/{{ item.arch }} · {{ formatTime(item.last_seen) }}</span>
          </div>
          <div class="row-actions">
            <button class="icon-button success" type="button" title="批准" @click="$emit('approve', item.id)">
              <Check :size="16" />
            </button>
            <button class="icon-button danger" type="button" title="拒绝" @click="$emit('reject', item.id)">
              <X :size="16" />
            </button>
          </div>
        </article>
      </section>

      <section v-if="adminTab === 'commands'" class="admin-section">
        <h2>快捷操作</h2>
        <form class="stack-form" @submit.prevent="$emit('createCommand')">
          <input v-model="commandForm.name" placeholder="名称，例如 重启 Nginx" />
          <input v-model="commandForm.command" placeholder="命令，例如 systemctl restart nginx" />
          <input v-model="commandForm.confirm_text" placeholder="确认提示，可选" />
          <button class="primary-button" type="submit"><Plus :size="16" />添加</button>
        </form>
        <article v-for="command in commands" :key="command.id" class="list-item">
          <div>
            <strong>{{ command.name }}</strong>
            <code>{{ command.command }}</code>
          </div>
          <button class="icon-button danger" type="button" title="停用" @click="$emit('removeCommand', command)">
            <Trash2 :size="15" />
          </button>
        </article>
        <h3>最近命令</h3>
        <article v-for="job in jobs.slice(0, 5)" :key="job.id" class="job-row">
          <span :class="['job-status', job.status]">{{ job.status }}</span>
          <strong>{{ job.command }}</strong>
          <small>{{ job.instance_id.slice(0, 8) }} · {{ formatTime(job.created_at) }}</small>
        </article>
      </section>

      <section v-if="adminTab === 'settings'" class="admin-section">
        <h2>系统设置</h2>
        <form class="stack-form" @submit.prevent="$emit('saveSettings')">
          <label>
            指标保留天数
            <input v-model.number="settingsForm.retention_days" min="1" max="365" type="number" />
          </label>
          <button class="primary-button" type="submit"><Settings :size="16" />保存设置</button>
        </form>
        <div class="background-control">
          <div class="background-preview" :class="{ empty: !settingsForm.background_image_url }">
            <img v-if="settingsForm.background_image_url" :src="settingsForm.background_image_url" alt="" />
            <Image v-else :size="28" />
          </div>
          <div class="background-actions">
            <label class="file-button">
              <Image :size="16" />
              上传背景
              <input accept="image/png,image/jpeg,image/webp" type="file" @change="$emit('selectBackgroundImage', $event)" />
            </label>
            <button
              class="text-button compact"
              type="button"
              :disabled="!settingsForm.background_image_url"
              @click="$emit('clearBackgroundImage')"
            >
              <X :size="15" />清除背景
            </button>
            <small>{{ backgroundFileName || 'PNG / JPEG / WebP，最大 5 MB' }}</small>
          </div>
        </div>
      </section>

      <section v-if="adminTab === 'logs'" class="admin-section">
        <h2>操作日志</h2>
        <article v-for="log in logs" :key="log.id" class="log-row">
          <strong>{{ log.action }}</strong>
          <span>{{ log.detail }}</span>
          <small>{{ log.actor }} · {{ formatTime(log.created_at) }}</small>
        </article>
      </section>
    </div>
  </aside>
</template>
