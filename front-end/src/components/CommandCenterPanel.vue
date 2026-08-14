<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import {
  CircleAlert,
  Clock3,
  History,
  LoaderCircle,
  PanelRightOpen,
  Plus,
  Search,
  ShieldCheck,
  Terminal,
  Trash2,
  X,
} from '@lucide/vue'
import type { CommandJob, CommandRecord } from '../types/domain'
import { formatTime } from '../utils/format'
import WorkspaceDrawer from './WorkspaceDrawer.vue'

const props = defineProps<{
  commands: CommandRecord[]
  jobs: CommandJob[]
  loading: boolean
  errorMessage: string
  commandForm: {
    name: string
    command: string
    confirm_text: string
  }
}>()

const emit = defineEmits<{
  createCommand: []
  removeCommand: [command: CommandRecord]
}>()

type CommandTab = 'library' | 'history'

const activeTab = ref<CommandTab>('library')
const commandSearch = ref('')
const jobSearch = ref('')
const jobStatus = ref('')
const createDrawerOpen = ref(false)
const createSubmitted = ref(false)
const selectedJobId = ref('')

const filteredCommands = computed(() => {
  const keyword = commandSearch.value.trim().toLowerCase()
  if (!keyword) return props.commands
  return props.commands.filter((command) => (
    command.name.toLowerCase().includes(keyword)
    || command.command.toLowerCase().includes(keyword)
    || command.confirm_text.toLowerCase().includes(keyword)
  ))
})

const filteredJobs = computed(() => {
  const keyword = jobSearch.value.trim().toLowerCase()
  return props.jobs.filter((job) => {
    if (jobStatus.value && job.status !== jobStatus.value) return false
    if (!keyword) return true
    return job.command.toLowerCase().includes(keyword)
      || job.instance_id.toLowerCase().includes(keyword)
      || job.requested_by.toLowerCase().includes(keyword)
      || job.id.toLowerCase().includes(keyword)
  })
})

const selectedJob = computed(() => (
  props.jobs.find((job) => job.id === selectedJobId.value) || null
))

const activeJobCount = computed(() => (
  props.jobs.filter((job) => !['completed', 'failed'].includes(job.status)).length
))

watch(
  () => [props.commandForm.name, props.commandForm.command],
  ([name, command]) => {
    if (!createSubmitted.value || name || command) return
    createSubmitted.value = false
    createDrawerOpen.value = false
  },
)

watch(
  () => props.jobs.map((job) => job.id).join('\u0000'),
  () => {
    if (selectedJobId.value && !selectedJob.value) selectedJobId.value = ''
  },
)

watch(
  () => filteredJobs.value.map((job) => job.id).join('\u0000'),
  () => {
    if (selectedJobId.value && !filteredJobs.value.some((job) => job.id === selectedJobId.value)) {
      selectedJobId.value = ''
    }
  },
)

function commandRunCount(commandId: string) {
  return props.jobs.filter((job) => job.command_id === commandId).length
}

function commandLastRun(commandId: string) {
  return props.jobs.find((job) => job.command_id === commandId) || null
}

function jobStatusLabel(status: string) {
  return {
    queued: '排队中',
    running: '执行中',
    completed: '已完成',
    failed: '失败',
  }[status] || status
}

function jobStatusClass(status: string) {
  return ['queued', 'running', 'completed', 'failed'].includes(status)
    ? status
    : 'unknown'
}

function jobDuration(job: CommandJob) {
  if (!job.completed_at) return ['completed', 'failed'].includes(job.status) ? '未知' : '进行中'
  const seconds = Math.max(0, job.completed_at - job.created_at)
  if (seconds < 60) return `${seconds} 秒`
  const minutes = Math.floor(seconds / 60)
  const remainder = seconds % 60
  return `${minutes} 分 ${remainder} 秒`
}

function clearCommandSearch() {
  commandSearch.value = ''
}

function clearJobSearch() {
  jobSearch.value = ''
}

function openCreateDrawer() {
  createSubmitted.value = false
  selectedJobId.value = ''
  createDrawerOpen.value = true
}

function closeCreateDrawer() {
  if (createSubmitted.value && props.loading) return
  createSubmitted.value = false
  createDrawerOpen.value = false
}

function submitCreateCommand() {
  createSubmitted.value = true
  emit('createCommand')
}

function selectJob(job: CommandJob) {
  createDrawerOpen.value = false
  selectedJobId.value = job.id
}

function closeJobDetails() {
  selectedJobId.value = ''
}

function handleTabKeydown(event: KeyboardEvent) {
  const tabs: CommandTab[] = ['library', 'history']
  const currentIndex = tabs.indexOf(activeTab.value)
  let nextIndex: number | null = null
  if (event.key === 'ArrowRight') nextIndex = (currentIndex + 1) % tabs.length
  if (event.key === 'ArrowLeft') nextIndex = (currentIndex - 1 + tabs.length) % tabs.length
  if (event.key === 'Home') nextIndex = 0
  if (event.key === 'End') nextIndex = tabs.length - 1
  if (nextIndex === null) return
  event.preventDefault()
  activeTab.value = tabs[nextIndex]
  const tab = (event.currentTarget as HTMLElement).querySelectorAll<HTMLButtonElement>('[role="tab"]')[nextIndex]
  void nextTick(() => tab?.focus())
}
</script>

<template>
  <section class="management-page command-workspace">
    <header class="command-workspace-header">
      <div class="command-workspace-heading-icon"><Terminal :size="21" /></div>
      <div class="command-workspace-heading">
        <span class="section-kicker">Remote actions</span>
        <h2>快捷命令</h2>
        <p>维护节点命令白名单，并查看最近执行结果</p>
      </div>
      <div class="command-workspace-header-actions">
        <span class="page-count">{{ commands.length }} 个命令</span>
        <button class="primary-button" type="button" :disabled="loading" @click="openCreateDrawer">
          <Plus :size="16" />新建命令
        </button>
      </div>
    </header>

    <div class="command-workspace-panel">
      <div class="command-workspace-tabs" role="tablist" aria-label="快捷命令视图" @keydown="handleTabKeydown">
        <button
          id="command-tab-library"
          type="button"
          role="tab"
          aria-controls="command-panel-library"
          :aria-selected="activeTab === 'library'"
          :tabindex="activeTab === 'library' ? 0 : -1"
          :class="{ active: activeTab === 'library' }"
          @click="activeTab = 'library'"
        >
          <Terminal :size="15" />命令库 <span>{{ commands.length }}</span>
        </button>
        <button
          id="command-tab-history"
          type="button"
          role="tab"
          aria-controls="command-panel-history"
          :aria-selected="activeTab === 'history'"
          :tabindex="activeTab === 'history' ? 0 : -1"
          :class="{ active: activeTab === 'history' }"
          @click="activeTab = 'history'"
        >
          <History :size="15" />执行记录 <span>{{ jobs.length }}</span>
          <em v-if="activeJobCount">{{ activeJobCount }} 进行中</em>
        </button>
      </div>

      <section
        v-if="activeTab === 'library'"
        id="command-panel-library"
        class="command-workspace-tab-panel"
        role="tabpanel"
        aria-labelledby="command-tab-library"
      >
        <div class="command-workspace-toolbar">
          <div class="command-workspace-search">
            <Search :size="15" aria-hidden="true" />
            <label class="command-workspace-sr-only" for="command-library-search">搜索命令库</label>
            <input id="command-library-search" v-model="commandSearch" type="search" placeholder="搜索名称、命令或确认提示" />
            <button
              v-if="commandSearch"
              type="button"
              title="清除搜索"
              aria-label="清除命令搜索"
              @click="clearCommandSearch"
            ><X :size="14" /></button>
          </div>
          <span>{{ filteredCommands.length }} / {{ commands.length }}</span>
        </div>

        <div class="command-workspace-records">
          <div v-if="filteredCommands.length === 0" class="command-workspace-empty">
            <span><Terminal :size="22" /></span>
            <strong>{{ commands.length ? '没有匹配的命令' : '暂无快捷命令' }}</strong>
            <p>{{ commands.length ? '调整搜索条件后重试。' : '创建后即可在节点操作面板中执行。' }}</p>
            <button v-if="commands.length === 0" class="primary-button" type="button" @click="openCreateDrawer">
              <Plus :size="15" />新建命令
            </button>
          </div>
          <div v-else class="command-library-table" role="table" aria-label="快捷命令库">
            <div class="command-library-head" role="row">
              <span role="columnheader">命令</span>
              <span role="columnheader">执行内容</span>
              <span role="columnheader">确认策略</span>
              <span role="columnheader">最近执行</span>
              <span role="columnheader">创建时间</span>
              <span role="columnheader" aria-label="操作"></span>
            </div>
            <div class="command-library-body" role="rowgroup">
              <article v-for="command in filteredCommands" :key="command.id" class="command-library-row" role="row">
                <div class="command-library-name" role="cell">
                  <span><Terminal :size="15" /></span>
                  <div><strong :title="command.name">{{ command.name }}</strong><small>最近记录 {{ commandRunCount(command.id) }} 次</small></div>
                </div>
                <div class="command-library-code" role="cell">
                  <small class="command-mobile-label">执行内容</small>
                  <code :title="command.command">{{ command.command }}</code>
                </div>
                <div class="command-library-confirm" role="cell">
                  <small class="command-mobile-label">确认策略</small>
                  <ShieldCheck v-if="command.confirm_text" :size="14" />
                  <CircleAlert v-else :size="14" />
                  <span :title="command.confirm_text || '使用默认命令内容确认'">
                    {{ command.confirm_text || '默认确认' }}
                  </span>
                </div>
                <div class="command-library-last-run" role="cell">
                  <small class="command-mobile-label">最近执行</small>
                  <template v-if="commandLastRun(command.id)">
                    <span :class="['command-job-status', jobStatusClass(commandLastRun(command.id)!.status)]">
                      {{ jobStatusLabel(commandLastRun(command.id)!.status) }}
                    </span>
                    <small>{{ formatTime(commandLastRun(command.id)!.created_at) }}</small>
                  </template>
                  <small v-else>尚未执行</small>
                </div>
                <div class="command-library-created" role="cell">
                  <small class="command-mobile-label">创建时间</small>
                  <time>{{ formatTime(command.created_at) }}</time>
                </div>
                <div class="command-library-action" role="cell">
                  <button
                    class="icon-button danger"
                    type="button"
                    title="停用快捷命令"
                    :aria-label="`停用快捷命令：${command.name}`"
                    :disabled="loading"
                    @click="$emit('removeCommand', command)"
                  ><Trash2 :size="15" /></button>
                </div>
              </article>
            </div>
          </div>
        </div>
      </section>

      <section
        v-else
        id="command-panel-history"
        class="command-workspace-tab-panel"
        role="tabpanel"
        aria-labelledby="command-tab-history"
      >
        <div class="command-workspace-toolbar command-history-toolbar">
          <div class="command-workspace-search">
            <Search :size="15" aria-hidden="true" />
            <label class="command-workspace-sr-only" for="command-history-search">搜索执行记录</label>
            <input id="command-history-search" v-model="jobSearch" type="search" placeholder="搜索命令、节点、操作者或任务 ID" />
            <button
              v-if="jobSearch"
              type="button"
              title="清除搜索"
              aria-label="清除执行记录搜索"
              @click="clearJobSearch"
            ><X :size="14" /></button>
          </div>
          <label class="command-history-status">
            <span class="command-workspace-sr-only">执行状态</span>
            <select v-model="jobStatus" title="按执行状态筛选">
              <option value="">全部状态</option>
              <option value="queued">排队中</option>
              <option value="running">执行中</option>
              <option value="completed">已完成</option>
              <option value="failed">失败</option>
            </select>
          </label>
          <span>{{ filteredJobs.length }} / {{ jobs.length }}</span>
        </div>

        <div class="command-workspace-records">
          <div v-if="filteredJobs.length === 0" class="command-workspace-empty">
            <span><History :size="22" /></span>
            <strong>{{ jobs.length ? '没有匹配的执行记录' : '暂无执行记录' }}</strong>
            <p>{{ jobs.length ? '调整搜索或状态条件后重试。' : '从节点操作面板运行命令后，结果会显示在这里。' }}</p>
          </div>
          <div v-else class="command-history-table" aria-label="快捷命令执行记录">
            <div class="command-history-head" aria-hidden="true">
              <span>状态</span>
              <span>命令</span>
              <span>节点</span>
              <span>操作者</span>
              <span>时间 / 总用时</span>
              <span>结果</span>
              <span></span>
            </div>
            <div class="command-history-body">
              <article
                v-for="job in filteredJobs"
                :key="job.id"
                class="command-history-row"
                :class="{ selected: selectedJobId === job.id }"
                role="button"
                tabindex="0"
                aria-haspopup="dialog"
                :aria-expanded="selectedJobId === job.id"
                :aria-label="`查看执行记录：${job.command}`"
                @click="selectJob(job)"
                @keydown.enter="selectJob(job)"
                @keydown.space.prevent="selectJob(job)"
              >
                <span :class="['command-job-status', jobStatusClass(job.status)]">{{ jobStatusLabel(job.status) }}</span>
                <div class="command-history-command"><strong :title="job.command">{{ job.command }}</strong><small :title="job.id">{{ job.id }}</small></div>
                <div class="command-history-instance"><small class="command-mobile-label">节点</small><code :title="job.instance_id">{{ job.instance_id }}</code></div>
                <div class="command-history-actor"><small class="command-mobile-label">操作者</small><span>{{ job.requested_by || '未知' }}</span></div>
                <div class="command-history-time"><small class="command-mobile-label">发起时间 / 总用时</small><time>{{ formatTime(job.created_at) }}</time><small>{{ jobDuration(job) }}</small></div>
                <div class="command-history-result"><strong>{{ job.exit_code ?? '-' }}</strong><small>退出码</small></div>
                <PanelRightOpen class="command-history-arrow" :size="16" aria-hidden="true" />
              </article>
            </div>
          </div>
        </div>
      </section>
    </div>

    <WorkspaceDrawer
      v-if="createDrawerOpen"
      title="新建快捷命令"
      description="添加后会立即进入节点操作面板的命令白名单。"
      size="medium"
      :modal="true"
      :busy="createSubmitted && loading"
      @close="closeCreateDrawer"
    >
      <form id="command-create-form" class="command-create-form" @submit.prevent="submitCreateCommand">
        <fieldset :disabled="createSubmitted && loading">
          <label>
            <span>显示名称</span>
            <input v-model.trim="commandForm.name" required autofocus autocomplete="off" placeholder="例如：重启 Nginx" />
          </label>
          <label>
            <span>执行命令</span>
            <textarea v-model.trim="commandForm.command" required rows="5" placeholder="systemctl restart nginx"></textarea>
          </label>
          <label>
            <span>确认提示 <i>可选</i></span>
            <textarea v-model.trim="commandForm.confirm_text" rows="4" placeholder="执行前显示的二次确认提示"></textarea>
          </label>
        </fieldset>
        <p v-if="createSubmitted && errorMessage && !loading" class="command-create-error" role="alert">
          <CircleAlert :size="15" />{{ errorMessage }}
        </p>
      </form>
      <template #footer>
        <button class="text-button" type="button" :disabled="createSubmitted && loading" @click="closeCreateDrawer">
          <X :size="15" />取消
        </button>
        <button class="primary-button" type="submit" form="command-create-form" :disabled="loading">
          <LoaderCircle v-if="createSubmitted && loading" class="spin" :size="16" />
          <Plus v-else :size="16" />{{ createSubmitted && loading ? '正在创建' : '创建命令' }}
        </button>
      </template>
    </WorkspaceDrawer>

    <WorkspaceDrawer
      v-if="selectedJob"
      :title="jobStatusLabel(selectedJob.status)"
      :description="`${formatTime(selectedJob.created_at)} · ${selectedJob.instance_id}`"
      size="wide"
      :modal="false"
      @close="closeJobDetails"
    >
      <div class="command-job-detail-summary">
        <span :class="['command-job-status', jobStatusClass(selectedJob.status)]" role="status" aria-live="polite">{{ jobStatusLabel(selectedJob.status) }}</span>
        <strong>{{ selectedJob.command }}</strong>
        <p><Clock3 :size="14" />{{ jobDuration(selectedJob) }}</p>
      </div>
      <dl class="command-job-detail-list">
        <div><dt>任务 ID</dt><dd>{{ selectedJob.id }}</dd></div>
        <div><dt>命令 ID</dt><dd>{{ selectedJob.command_id || '无' }}</dd></div>
        <div><dt>节点 ID</dt><dd>{{ selectedJob.instance_id }}</dd></div>
        <div><dt>操作者</dt><dd>{{ selectedJob.requested_by || '未知' }}</dd></div>
        <div><dt>发起时间</dt><dd>{{ formatTime(selectedJob.created_at) }}</dd></div>
        <div><dt>完成时间</dt><dd>{{ selectedJob.completed_at ? formatTime(selectedJob.completed_at) : '未完成' }}</dd></div>
        <div><dt>退出码</dt><dd>{{ selectedJob.exit_code ?? '无' }}</dd></div>
        <div><dt>总用时</dt><dd>{{ jobDuration(selectedJob) }}</dd></div>
        <div class="command-job-detail-wide"><dt>执行命令</dt><dd><code>{{ selectedJob.command }}</code></dd></div>
        <div class="command-job-detail-wide"><dt>命令输出</dt><dd><pre>{{ selectedJob.output || '暂无输出' }}</pre></dd></div>
      </dl>
    </WorkspaceDrawer>
  </section>
</template>
