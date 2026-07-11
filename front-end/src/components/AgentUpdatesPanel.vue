<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue'
import {
  Check,
  CircleAlert,
  Clock3,
  FileArchive,
  LoaderCircle,
  PackageCheck,
  Pencil,
  Plus,
  RotateCcw,
  Save,
  Send,
  ShieldAlert,
  Trash2,
  Upload,
  X,
} from 'lucide-vue-next'
import type {
  AgentArtifactTarget,
  AgentRelease,
  AgentReleaseForm,
  AgentUpdateAttempt,
  AgentUpdateAttemptStatus,
  Instance,
} from '../types/domain'
import { formatBytes, formatTime } from '../utils/format'

const props = defineProps<{
  instances: Instance[]
  releases: AgentRelease[]
  attempts: AgentUpdateAttempt[]
  form: AgentReleaseForm
  operation: string | null
  busyId: string
  message: string
}>()

const emit = defineEmits<{
  createRelease: []
  saveRelease: [releaseId: string, form: AgentReleaseForm]
  uploadArtifact: [releaseId: string, target: AgentArtifactTarget, file: File]
  deleteArtifact: [releaseId: string, artifactId: string]
  publishRelease: [release: AgentRelease]
  deleteRelease: [release: AgentRelease]
  retryAttempt: [attempt: AgentUpdateAttempt]
}>()

const draftEdits = reactive<Record<string, AgentReleaseForm>>({})
const artifactTargets = reactive<Record<string, AgentArtifactTarget>>({})
const artifactFiles = reactive<Record<string, File | null>>({})
const fileInputKeys = reactive<Record<string, number>>({})
const editingReleaseId = ref<string | null>(null)

const nativeArchitecturesByOs: Record<string, string[]> = {
  linux: ['x86_64', 'aarch64', 'arm', 'x86'],
  windows: ['x64', 'arm64', 'x86'],
  macos: ['arm64', 'x86_64'],
}

const releaseStatusText: Record<AgentRelease['status'], string> = {
  draft: '草稿',
  published: '已发布',
}

const attemptStatusText: Record<AgentUpdateAttemptStatus, string> = {
  pending: '等待安排',
  waiting: '等待执行',
  downloading: '下载中',
  verifying: '校验中',
  waiting_idle: '等待空闲',
  installing: '安装中',
  awaiting_restart: '等待重连',
  succeeded: '已完成',
  rollback_succeeded: '已回滚',
  failed: '失败',
}

const publishedCount = computed(() => props.releases.filter((release) => release.status === 'published').length)

watch(
  () => props.releases,
  (releases) => {
    for (const release of releases) {
      if (!draftEdits[release.id]) {
        draftEdits[release.id] = { version: release.version, notes: release.notes }
      }
      if (!artifactTargets[release.id]) {
        artifactTargets[release.id] = { os: 'linux', package_type: 'standalone', native_arch: 'x86_64' }
      }
      if (!(release.id in artifactFiles)) artifactFiles[release.id] = null
      if (!(release.id in fileInputKeys)) fileInputKeys[release.id] = 0
    }
  },
  { immediate: true },
)

watch(
  () => props.operation,
  (operation) => {
    if (!operation && props.message && editingReleaseId.value) {
      editingReleaseId.value = null
    }
  },
)

function artifactAccept(target: AgentArtifactTarget) {
  return target.os === 'windows' ? '.exe' : '.bin'
}

function nativeArchitectures(os: string) {
  const defaults = nativeArchitecturesByOs[os] || nativeArchitecturesByOs.linux
  const detectedArchitectures = props.instances
    .filter((instance) => instance.package_type === 'standalone' && (os === 'linux' ? !['windows', 'macos'].includes(instance.os) : instance.os === os))
    .map((instance) => instance.native_arch?.trim())
    .filter((architecture): architecture is string => Boolean(architecture))
  return [...new Set([...defaults, ...detectedArchitectures])]
}

function syncArtifactArchitecture(releaseId: string) {
  const target = artifactTargets[releaseId]
  target.package_type = 'standalone'
  const architectures = nativeArchitectures(target.os)
  if (!architectures.includes(target.native_arch)) target.native_arch = architectures[0]
}

function changeArtifactOs(releaseId: string) {
  syncArtifactArchitecture(releaseId)
}

function chooseArtifactFile(releaseId: string, event: Event) {
  const input = event.target as HTMLInputElement
  artifactFiles[releaseId] = input.files?.[0] || null
}

function submitArtifact(releaseId: string) {
  const file = artifactFiles[releaseId]
  if (!file) return
  emit('uploadArtifact', releaseId, { ...artifactTargets[releaseId] }, file)
  artifactFiles[releaseId] = null
  fileInputKeys[releaseId] += 1
}

function editRelease(release: AgentRelease) {
  draftEdits[release.id] = { version: release.version, notes: release.notes }
  editingReleaseId.value = release.id
}

function cancelEdit(releaseId: string) {
  const release = props.releases.find((item) => item.id === releaseId)
  if (release) draftEdits[releaseId] = { version: release.version, notes: release.notes }
  editingReleaseId.value = null
}

function saveRelease(releaseId: string) {
  emit('saveRelease', releaseId, { ...draftEdits[releaseId] })
}

function attemptsFor(release: AgentRelease) {
  return release.attempts.length
    ? release.attempts
    : props.attempts.filter((attempt) => attempt.release_id === release.id)
}

function isBusy(id: string) {
  return Boolean(props.operation) && props.busyId === id
}
</script>

<template>
  <section class="management-page updates-page">
    <header class="page-header">
      <div class="page-heading-icon cyan"><PackageCheck :size="22" /></div>
      <div>
        <span class="section-kicker">Agent releases</span>
        <h2>程序更新</h2>
        <p>维护实例端可执行文件，并跟踪每台实例的更新结果。</p>
      </div>
      <span class="page-count">{{ publishedCount }} 个已发布</span>
    </header>

    <p v-if="message" class="update-feedback" role="status" aria-live="polite">
      <Check :size="15" />{{ message }}
    </p>

    <section class="admin-content-card release-create-card" aria-labelledby="create-agent-release-title">
      <div class="card-heading">
        <div>
          <h3 id="create-agent-release-title">创建更新草稿</h3>
          <p>为一个版本添加对应系统和架构的standalone 可执行文件。</p>
        </div>
      </div>
      <form class="release-create-form" @submit.prevent="$emit('createRelease')">
        <label>
          <span>版本号</span>
          <input v-model.trim="form.version" required placeholder="例如：1.4.0" autocomplete="off" />
        </label>
        <label>
          <span>发布说明 <i>可选</i></span>
          <textarea v-model.trim="form.notes" placeholder="本次更新内容"></textarea>
        </label>
        <button class="primary-button" type="submit" :disabled="Boolean(operation)">
          <LoaderCircle v-if="operation === 'creating'" class="spin" :size="16" />
          <Plus v-else :size="16" />
          {{ operation === 'creating' ? '正在创建' : '创建草稿' }}
        </button>
      </form>
    </section>

    <section class="release-list" aria-labelledby="agent-release-list-title">
      <div class="release-list-heading">
        <div>
          <span class="section-kicker">Release history</span>
          <h3 id="agent-release-list-title">版本与更新状态</h3>
        </div>
        <span>{{ releases.length }} 个版本</span>
      </div>

      <div v-if="releases.length === 0" class="page-empty update-empty">
        <span><PackageCheck :size="24" /></span>
        <strong>暂无 Agent 更新版本</strong>
        <p>创建草稿后即可上传面向不同实例的可执行文件。</p>
      </div>

      <article v-for="release in releases" :key="release.id" class="update-release-card">
        <header class="release-card-header">
          <div class="release-identity">
            <span :class="['release-status', release.status]">{{ releaseStatusText[release.status] }}</span>
            <div v-if="editingReleaseId !== release.id">
              <h3>Agent {{ release.version }}</h3>
              <p v-if="release.notes" class="release-notes">{{ release.notes }}</p>
              <p v-else class="release-notes empty">未填写发布说明</p>
            </div>
          </div>

          <div v-if="editingReleaseId !== release.id" class="release-actions">
            <time :title="formatTime(release.published_at || release.created_at)">
              {{ release.status === 'published' ? `发布于 ${formatTime(release.published_at)}` : `创建于 ${formatTime(release.created_at)}` }}
            </time>
            <template v-if="release.status === 'draft'">
              <button
                class="icon-button"
                type="button"
                title="编辑草稿"
                :aria-label="`编辑 Agent ${release.version} 草稿`"
                :disabled="Boolean(operation)"
                @click="editRelease(release)"
              >
                <Pencil :size="15" />
              </button>
              <button
                class="primary-button release-publish-button"
                type="button"
                :title="release.artifacts.length ? '发布更新' : '至少添加一个可执行文件后才能发布'"
                :disabled="Boolean(operation) || release.artifacts.length === 0"
                @click="$emit('publishRelease', release)"
              >
                <LoaderCircle v-if="isBusy(release.id) && operation === 'publishing'" class="spin" :size="15" />
                <Send v-else :size="15" />发布
              </button>
              <button
                class="icon-button danger"
                type="button"
                title="删除草稿"
                :aria-label="`删除 Agent ${release.version} 草稿`"
                :disabled="Boolean(operation)"
                @click="$emit('deleteRelease', release)"
              >
                <Trash2 :size="15" />
              </button>
            </template>
          </div>
        </header>

        <form
          v-if="editingReleaseId === release.id"
          class="release-edit-form"
          @submit.prevent="saveRelease(release.id)"
        >
          <label>
            <span>版本号</span>
            <input v-model.trim="draftEdits[release.id].version" required autocomplete="off" />
          </label>
          <label>
            <span>发布说明 <i>可选</i></span>
            <textarea v-model.trim="draftEdits[release.id].notes"></textarea>
          </label>
          <div class="release-edit-actions">
            <button class="text-button" type="button" :disabled="Boolean(operation)" @click="cancelEdit(release.id)">
              <X :size="15" />取消
            </button>
            <button class="primary-button" type="submit" :disabled="Boolean(operation)">
              <LoaderCircle v-if="isBusy(release.id) && operation === 'saving'" class="spin" :size="15" />
              <Save v-else :size="15" />保存草稿
            </button>
          </div>
        </form>

        <div class="release-coverage" :aria-label="`Agent ${release.version} 覆盖情况`">
          <span><strong>{{ release.coverage.covered_instances }}</strong> / {{ release.coverage.eligible_instances }} 已覆盖</span>
          <span :class="{ warning: release.coverage.missing_artifact_instances > 0 }">
            <CircleAlert :size="14" />{{ release.coverage.missing_artifact_instances }} 缺少可执行文件
          </span>
          <span :class="{ warning: release.coverage.unprivileged_instances > 0 }">
            <ShieldAlert :size="14" />{{ release.coverage.unprivileged_instances }} 权限不足
          </span>
        </div>

        <section class="release-artifacts" :aria-labelledby="`artifacts-${release.id}`">
          <div class="release-section-heading">
            <div>
              <h4 :id="`artifacts-${release.id}`">可执行文件</h4>
              <span>{{ release.artifacts.length }} 个目标</span>
            </div>
          </div>

          <div v-if="release.artifacts.length === 0" class="release-empty-row">
            <FileArchive :size="17" />尚未添加可执行文件
          </div>
          <div v-else class="artifact-list">
            <article v-for="artifact in release.artifacts" :key="artifact.id" class="artifact-row">
              <span class="artifact-icon"><FileArchive :size="17" /></span>
              <div class="artifact-name">
                <strong :title="artifact.file_name">{{ artifact.file_name }}</strong>
                <span>{{ artifact.os }} / {{ artifact.native_arch }} / {{ artifact.package_type }}</span>
              </div>
              <div class="artifact-integrity">
                <span>{{ formatBytes(artifact.size_bytes) }}</span>
                <code :title="artifact.sha256">{{ artifact.sha256.slice(0, 12) }}</code>
              </div>
              <button
                v-if="release.status === 'draft'"
                class="icon-button danger"
                type="button"
                title="移除可执行文件"
                :aria-label="`移除 ${artifact.file_name}`"
                :disabled="Boolean(operation)"
                @click="$emit('deleteArtifact', release.id, artifact.id)"
              >
                <LoaderCircle v-if="isBusy(artifact.id) && operation === 'deleting'" class="spin" :size="15" />
                <Trash2 v-else :size="15" />
              </button>
            </article>
          </div>

          <form v-if="release.status === 'draft'" class="artifact-upload-form" @submit.prevent="submitArtifact(release.id)">
            <label>
              <span>目标系统</span>
              <select v-model="artifactTargets[release.id].os" @change="changeArtifactOs(release.id)">
                <option value="linux">Linux</option>
                <option value="windows">Windows</option>
                <option value="macos">macOS</option>
              </select>
            </label>
            <label>
              <span>分发格式</span>
              <input value="standalone" disabled />
            </label>
            <label>
              <span>原生架构</span>
              <select v-model="artifactTargets[release.id].native_arch" required>
                <option
                  v-for="architecture in nativeArchitectures(artifactTargets[release.id].os)"
                  :key="architecture"
                  :value="architecture"
                >
                  {{ architecture }}
                </option>
              </select>
            </label>
            <label class="artifact-file-picker">
              <span>可执行文件文件</span>
              <span class="file-button">
                <Upload :size="15" />{{ artifactFiles[release.id]?.name || '选择可执行文件' }}
                <input
                  :key="fileInputKeys[release.id]"
                  type="file"
                  required
                  :accept="artifactAccept(artifactTargets[release.id])"
                  :disabled="Boolean(operation)"
                  @change="chooseArtifactFile(release.id, $event)"
                />
              </span>
            </label>
            <button class="text-button artifact-upload-button" type="submit" :disabled="Boolean(operation) || !artifactFiles[release.id]">
              <LoaderCircle v-if="isBusy(release.id) && operation === 'uploading'" class="spin" :size="15" />
              <Upload v-else :size="15" />上传
            </button>
          </form>
        </section>

        <section class="release-attempts" :aria-labelledby="`attempts-${release.id}`">
          <div class="release-section-heading">
            <div>
              <h4 :id="`attempts-${release.id}`">实例更新</h4>
              <span>{{ attemptsFor(release).length }} 条记录</span>
            </div>
          </div>

          <div v-if="attemptsFor(release).length === 0" class="release-empty-row">
            <Clock3 :size="17" />尚无实例更新记录
          </div>
          <div v-else class="update-attempts-table">
            <div class="update-attempts-head">
              <span>实例</span><span>版本</span><span>状态</span><span>更新时间</span><span></span>
            </div>
            <article v-for="attempt in attemptsFor(release)" :key="attempt.id" class="update-attempt-row">
              <strong :title="attempt.instance_id">{{ attempt.instance_id.slice(0, 12) }}</strong>
              <span class="attempt-versions">{{ attempt.from_version }} -&gt; {{ attempt.target_version }}</span>
              <span :class="['attempt-status', attempt.status]" :title="attempt.message || attemptStatusText[attempt.status]">
                {{ attemptStatusText[attempt.status] }}
              </span>
              <time>{{ formatTime(attempt.updated_at) }}</time>
              <button
                v-if="attempt.status === 'failed' || attempt.status === 'rollback_succeeded'"
                class="icon-button"
                type="button"
                title="重新尝试更新"
                :aria-label="`重新尝试实例 ${attempt.instance_id} 的更新`"
                :disabled="Boolean(operation)"
                @click="$emit('retryAttempt', attempt)"
              >
                <LoaderCircle v-if="isBusy(attempt.id) && operation === 'retrying'" class="spin" :size="15" />
                <RotateCcw v-else :size="15" />
              </button>
            </article>
          </div>
        </section>
      </article>
    </section>
  </section>
</template>
