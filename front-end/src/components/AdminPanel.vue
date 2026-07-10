<script setup lang="ts">
import {
  Check,
  ClipboardList,
  Image,
  ListChecks,
  LoaderCircle,
  Plus,
  Settings,
  Terminal,
  Trash2,
  X,
} from 'lucide-vue-next'
import type { ActionLog, AdminTab, CommandJob, CommandRecord, PendingInstance } from '../types/domain'
import { formatTime } from '../utils/format'

defineProps<{
  adminTab: AdminTab
  pendingInstances: PendingInstance[]
  commands: CommandRecord[]
  jobs: CommandJob[]
  logs: ActionLog[]
  settingsForm: {
    retention_days: number
    background_image_url: string | null
  }
  backgroundFileName: string
  backgroundOperation: 'uploading' | 'removing' | null
  backgroundMessage: string
  commandForm: {
    name: string
    command: string
    confirm_text: string
  }
}>()

defineEmits<{
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
  <section class="management-page">
    <template v-if="adminTab === 'pending'">
      <header class="page-header">
        <div class="page-heading-icon"><ListChecks :size="22" /></div>
        <div>
          <span class="section-kicker">Access review</span>
          <h2>接入审核</h2>
          <p>审核新节点的接入请求，仅批准可信设备。</p>
        </div>
        <span class="page-count">{{ pendingInstances.length }} 个待处理</span>
      </header>

      <div class="admin-content-card wide-card">
        <div class="card-heading">
          <div><h3>待审批节点</h3><p>节点在获得批准之前不会出现在公开实例列表中。</p></div>
        </div>
        <Transition name="content" mode="out-in">
          <div v-if="pendingInstances.length === 0" key="empty" class="page-empty">
            <span><Check :size="24" /></span>
            <strong>所有申请均已处理</strong>
            <p>目前没有等待审核的新节点。</p>
          </div>
          <TransitionGroup v-else key="approvals" name="row" tag="div" class="approval-list">
            <article v-for="item in pendingInstances" :key="item.id" class="approval-row">
              <span class="list-icon"><Terminal :size="17" /></span>
              <div class="approval-identity">
                <strong>{{ item.hostname }}</strong>
                <span>{{ item.os }}/{{ item.arch }} · Agent {{ item.agent_version }}</span>
              </div>
              <div class="approval-time">
                <span>最后请求</span>
                <strong>{{ formatTime(item.last_seen) }}</strong>
              </div>
              <div class="row-actions">
                <button class="approve-button" type="button" @click="$emit('approve', item.id)">
                  <Check :size="15" />批准
                </button>
                <button class="reject-button" type="button" @click="$emit('reject', item.id)">
                  <X :size="15" />拒绝
                </button>
              </div>
            </article>
          </TransitionGroup>
        </Transition>
      </div>
    </template>

    <template v-if="adminTab === 'commands'">
      <header class="page-header">
        <div class="page-heading-icon purple"><Terminal :size="22" /></div>
        <div>
          <span class="section-kicker">Remote actions</span>
          <h2>快捷命令</h2>
          <p>维护可在实例节点上安全执行的预设操作。</p>
        </div>
        <span class="page-count">{{ commands.length }} 个已启用</span>
      </header>

      <div class="admin-page-grid commands-layout">
        <div class="admin-content-card">
          <div class="card-heading"><div><h3>创建快捷命令</h3><p>添加后可直接在主页节点卡片中执行。</p></div></div>
          <form class="stack-form page-form" @submit.prevent="$emit('createCommand')">
            <label><span>显示名称</span><input v-model="commandForm.name" required placeholder="例如：重启 Nginx" /></label>
            <label><span>执行命令</span><input v-model="commandForm.command" required placeholder="systemctl restart nginx" /></label>
            <label><span>确认提示 <i>可选</i></span><input v-model="commandForm.confirm_text" placeholder="执行前显示的二次确认提示" /></label>
            <button class="primary-button" type="submit"><Plus :size="16" />添加快捷命令</button>
          </form>
        </div>

        <div class="admin-content-card">
          <div class="card-heading"><div><h3>已启用命令</h3><p>当前允许执行的命令白名单。</p></div></div>
          <Transition name="content" mode="out-in">
            <div v-if="commands.length === 0" key="empty" class="compact-empty">暂无快捷命令</div>
            <TransitionGroup v-else key="commands" name="row" tag="div" class="command-list">
              <article v-for="command in commands" :key="command.id" class="command-row">
                <span class="list-icon"><Terminal :size="16" /></span>
                <div><strong>{{ command.name }}</strong><code>{{ command.command }}</code></div>
                <button class="icon-button danger" type="button" title="停用" @click="$emit('removeCommand', command)">
                  <Trash2 :size="15" />
                </button>
              </article>
            </TransitionGroup>
          </Transition>
        </div>

        <div class="admin-content-card recent-jobs-card">
          <div class="card-heading"><div><h3>最近执行记录</h3><p>最近提交到节点的命令任务。</p></div></div>
          <Transition name="content" mode="out-in">
            <div v-if="jobs.length === 0" key="empty" class="compact-empty">暂无执行记录</div>
            <TransitionGroup v-else key="jobs" name="row" tag="div" class="jobs-table">
              <article v-for="job in jobs.slice(0, 10)" :key="job.id" class="job-table-row">
                <span :class="['job-status', job.status]">{{ job.status }}</span>
                <strong>{{ job.command }}</strong>
                <small>节点 {{ job.instance_id.slice(0, 8) }}</small>
                <time>{{ formatTime(job.created_at) }}</time>
              </article>
            </TransitionGroup>
          </Transition>
        </div>
      </div>
    </template>

    <template v-if="adminTab === 'settings'">
      <header class="page-header">
        <div class="page-heading-icon amber"><Settings :size="22" /></div>
        <div>
          <span class="section-kicker">System preferences</span>
          <h2>系统设置</h2>
          <p>管理数据生命周期与监控页面的视觉外观。</p>
        </div>
      </header>

      <div class="admin-page-grid settings-layout">
        <div class="admin-content-card">
          <div class="card-heading"><div><h3>数据保留</h3><p>超过保留期限的历史指标会被自动清理。</p></div></div>
          <form class="stack-form page-form" @submit.prevent="$emit('saveSettings')">
            <label>
              <span>指标保留天数</span>
              <input v-model.number="settingsForm.retention_days" min="1" max="365" type="number" />
            </label>
            <small class="form-help">可设置 1 至 365 天，不影响节点基础信息。</small>
            <button class="primary-button" type="submit"><Settings :size="16" />保存设置</button>
          </form>
        </div>

        <div class="admin-content-card background-card">
          <div class="card-heading"><div><h3>页面背景</h3><p>自定义监控页面背景，系统会自动添加深色遮罩。</p></div></div>
          <div :class="['large-background-preview', { empty: !settingsForm.background_image_url }]">
            <Transition name="fade" mode="out-in">
              <img
                v-if="settingsForm.background_image_url"
                key="image"
                :src="settingsForm.background_image_url"
                alt="当前背景"
              />
              <div v-else key="empty"><Image :size="28" /><span>当前使用默认渐变背景</span></div>
            </Transition>
          </div>
          <div class="background-toolbar">
            <label :class="['file-button', { disabled: backgroundOperation }]">
              <LoaderCircle v-if="backgroundOperation === 'uploading'" class="spin" :size="15" />
              <Image v-else :size="15" />
              {{ backgroundOperation === 'uploading' ? '正在上传' : '选择图片' }}
              <input
                type="file"
                accept="image/png,image/jpeg,image/webp"
                :disabled="Boolean(backgroundOperation)"
                @change="$emit('selectBackgroundImage', $event)"
              />
            </label>
            <Transition name="fade-scale">
              <button
                v-if="settingsForm.background_image_url"
                class="text-button danger"
                type="button"
                :disabled="Boolean(backgroundOperation)"
                @click="$emit('clearBackgroundImage')"
              >
                <LoaderCircle v-if="backgroundOperation === 'removing'" class="spin" :size="14" />
                <Trash2 v-else :size="14" />
                {{ backgroundOperation === 'removing' ? '正在移除' : '移除背景' }}
              </button>
            </Transition>
            <small :class="{ success: backgroundMessage }">{{ backgroundMessage || backgroundFileName || '支持 PNG、JPEG、WebP，最大 5MB' }}</small>
          </div>
        </div>
      </div>
    </template>

    <template v-if="adminTab === 'logs'">
      <header class="page-header">
        <div class="page-heading-icon cyan"><ClipboardList :size="22" /></div>
        <div>
          <span class="section-kicker">Audit trail</span>
          <h2>操作日志</h2>
          <p>查看管理员最近执行的审批、配置和节点操作。</p>
        </div>
        <span class="page-count">最近 {{ Math.min(logs.length, 20) }} 条</span>
      </header>

      <div class="admin-content-card wide-card">
        <div class="card-heading"><div><h3>管理操作记录</h3><p>按操作时间倒序显示。</p></div></div>
        <Transition name="content" mode="out-in">
          <div v-if="logs.length === 0" key="empty" class="page-empty">
            <span><ClipboardList :size="24" /></span><strong>暂无操作记录</strong>
          </div>
          <TransitionGroup v-else key="logs" name="row" tag="div" class="logs-table">
            <div key="header" class="logs-table-head"><span>操作</span><span>详细信息</span><span>目标</span><span>时间</span></div>
            <article v-for="log in logs.slice(0, 20)" :key="log.id" class="logs-table-row">
              <strong>{{ log.action }}</strong>
              <p>{{ log.detail }}</p>
              <span>{{ log.target }}</span>
              <time>{{ formatTime(log.created_at) }}</time>
            </article>
          </TransitionGroup>
        </Transition>
      </div>
    </template>
  </section>
</template>
