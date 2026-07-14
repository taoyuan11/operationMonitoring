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
  AgentArtifactUploadItem,
  AgentArtifactUploadResult,
  AgentArtifactUploadRow,
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
  createRelease: [onCreated: (releaseId: string) => void]
  saveRelease: [releaseId: string, form: AgentReleaseForm]
  uploadArtifact: [releaseId: string, uploads: AgentArtifactUploadItem[], onComplete: (result: AgentArtifactUploadResult) => void]
  deleteArtifact: [releaseId: string, artifactId: string]
  publishRelease: [release: AgentRelease]
  deleteRelease: [release: AgentRelease]
  retryAttempt: [attempt: AgentUpdateAttempt]
}>()

const draftEdits = reactive<Record<string, AgentReleaseForm>>({})
const artifactUploadRows = reactive<Record<string, AgentArtifactUploadRow[]>>({})
const batchFileErrors = reactive<Record<string, string>>({})
const batchDragDepths = reactive<Record<string, number>>({})
const batchDragActive = reactive<Record<string, boolean>>({})
const batchInputKeys = reactive<Record<string, number>>({})
const fileInputKeys = reactive<Record<string, number>>({})
const checksumInputKeys = reactive<Record<string, number>>({})
const editingReleaseId = ref<string | null>(null)
const createReleaseFiles = ref<File[]>([])
const createReleaseFileError = ref('')
const createReleaseVersionSource = ref('')
const createReleaseInputKey = ref(0)
let uploadRowSequence = 0

const nativeArchitecturesByOs: Record<string, string[]> = {
  linux: ['x86_64', 'x86_64-musl', 'aarch64', 'arm', 'x86'],
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
const createReleaseFileSummary = computed(() => {
  const packageCount = createReleaseFiles.value.filter((file) => !file.name.toLowerCase().endsWith('.sha256')).length
  const checksumCount = createReleaseFiles.value.length - packageCount
  if (!createReleaseFiles.value.length) return '选择更新包'
  return `${packageCount} 个更新包，${checksumCount} 个校验文件`
})

watch(
  () => props.releases,
  (releases) => {
    for (const release of releases) {
      if (!draftEdits[release.id]) {
        draftEdits[release.id] = { version: release.version, notes: release.notes }
      }
      if (!(release.id in artifactUploadRows)) artifactUploadRows[release.id] = [createUploadRow('linux', 'x86_64')]
      if (!(release.id in batchFileErrors)) batchFileErrors[release.id] = ''
      if (!(release.id in batchDragDepths)) batchDragDepths[release.id] = 0
      if (!(release.id in batchDragActive)) batchDragActive[release.id] = false
      if (!(release.id in batchInputKeys)) batchInputKeys[release.id] = 0
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

function createUploadRow(os = 'linux', nativeArch = 'x86_64'): AgentArtifactUploadRow {
  uploadRowSequence += 1
  return {
    id: `artifact-upload-${Date.now()}-${uploadRowSequence}`,
    os,
    package_type: 'standalone',
    native_arch: nativeArch,
    file: null,
    checksum_file: null,
    error: '',
    inference: 'manual',
  }
}

function artifactAccept(target: AgentArtifactTarget) {
  return target.os === 'windows' ? '.exe' : '.bin'
}

function uploadRowsFor(releaseId: string) {
  return artifactUploadRows[releaseId] || []
}

function uploadableRowsFor(releaseId: string) {
  return uploadRowsFor(releaseId).filter((row) => row.file && row.checksum_file && row.os && row.native_arch)
}

function nativeArchitectures(os: string) {
  const defaults = nativeArchitecturesByOs[os] || []
  const detectedArchitectures = props.instances
    .filter((instance) => instance.package_type === 'standalone' && (os === 'linux' ? !['windows', 'macos'].includes(instance.os) : instance.os === os))
    .map((instance) => instance.native_arch?.trim())
    .filter((architecture): architecture is string => Boolean(architecture))
  return [...new Set([...defaults, ...detectedArchitectures])]
}

function syncArtifactArchitecture(row: AgentArtifactUploadRow) {
  row.package_type = 'standalone'
  const architectures = nativeArchitectures(row.os)
  if (row.native_arch && !architectures.includes(row.native_arch)) row.native_arch = architectures[0]
  row.inference = 'manual'
}

function changeArtifactOs(row: AgentArtifactUploadRow) {
  syncArtifactArchitecture(row)
  if (row.file) setUploadRowFile(row, row.file)
}

function changeArtifactArchitecture(row: AgentArtifactUploadRow) {
  row.inference = 'manual'
  row.error = ''
}

function chooseArtifactFile(row: AgentArtifactUploadRow, event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (file) setUploadRowFile(row, file)
}

function setUploadRowFile(row: AgentArtifactUploadRow, file: File) {
  const expectedExtension = artifactAccept(row)
  if (!file.name.toLowerCase().endsWith(expectedExtension)) {
    row.file = null
    row.error = `请选择 ${expectedExtension} 可执行文件`
    fileInputKeys[row.id] = (fileInputKeys[row.id] || 0) + 1
    return
  }
  row.file = file
  if (row.checksum_file && !checksumMatchesFile(file, row.checksum_file)) {
    row.checksum_file = null
    checksumInputKeys[row.id] = (checksumInputKeys[row.id] || 0) + 1
  }
  row.error = row.checksum_file ? '' : '请选择同名 .sha256 校验文件'
  row.inference = 'manual'
}

function chooseChecksumFile(row: AgentArtifactUploadRow, event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (!file) return
  if (!file.name.toLowerCase().endsWith('.sha256')) {
    row.checksum_file = null
    row.error = '请选择 .sha256 校验文件'
    checksumInputKeys[row.id] = (checksumInputKeys[row.id] || 0) + 1
    return
  }
  if (row.file && !checksumMatchesFile(row.file, file)) {
    row.checksum_file = null
    row.error = '校验文件名必须为可执行文件名加 .sha256'
    checksumInputKeys[row.id] = (checksumInputKeys[row.id] || 0) + 1
    return
  }
  row.checksum_file = file
  row.error = row.file ? '' : '请先选择对应的可执行文件'
}

function checksumMatchesFile(file: File, checksumFile: File) {
  return checksumFile.name.toLowerCase() === `${file.name.toLowerCase()}.sha256`
}

function addUploadRow(releaseId: string, os = 'linux', nativeArch = 'x86_64') {
  artifactUploadRows[releaseId].push(createUploadRow(os, nativeArch))
}

function removeUploadRow(releaseId: string, rowId: string) {
  const rows = artifactUploadRows[releaseId]
  if (rows.length <= 1) {
    rows[0].file = null
    rows[0].checksum_file = null
    rows[0].error = ''
    rows[0].inference = 'manual'
    return
  }
  artifactUploadRows[releaseId] = rows.filter((row) => row.id !== rowId)
}

function inferTargetFromName(fileName: string) {
  const name = fileName.toLowerCase()
  const os = name.endsWith('.exe')
    ? 'windows'
    : /macos|darwin|osx/.test(name)
      ? 'macos'
      : /linux/.test(name)
        ? 'linux'
        : ''
  const detectedArchitecture = /x86_64[-_]musl/.test(name)
    ? 'x86_64-musl'
    : /aarch64|arm64/.test(name)
    ? os === 'windows' || os === 'macos' ? 'arm64' : 'aarch64'
    : /x86_64|amd64|x64/.test(name)
      ? os === 'windows' ? 'x64' : 'x86_64'
      : /armv?7|\barm\b/.test(name)
        ? 'arm'
        : /i[3-6]86|\bx86\b/.test(name)
          ? 'x86'
          : ''
  const architecture = detectedArchitecture === 'arm' && (os === 'windows' || os === 'macos')
    ? 'arm64'
    : detectedArchitecture === 'x86' && os === 'macos'
      ? 'x86_64'
      : detectedArchitecture
  return {
    os,
    native_arch: architecture,
    inference: os && architecture ? 'matched' as const : os ? 'needs_architecture' as const : 'needs_target' as const,
  }
}

function inferVersionFromName(fileName: string) {
  const baseName = fileName
    .replace(/\.sha256$/i, '')
    .replace(/\.(?:bin|exe)$/i, '')
    .replace(/(?:[_-](?:linux|windows|win|macos|darwin|osx))?(?:[_-](?:x86_64[-_]musl|x86_64|amd64|x64|aarch64|arm64|armv?7|arm|i[3-6]86|x86))$/i, '')
  const match = baseName.match(
    /(?:^|[_-])v?((?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?)$/,
  )
  return match?.[1] || ''
}

function chooseCreateReleaseFiles(event: Event) {
  const input = event.target as HTMLInputElement
  const selectedFiles = Array.from(input.files || [])
  const validFiles = selectedFiles.filter((file) => {
    const name = file.name.toLowerCase()
    return name.endsWith('.bin') || name.endsWith('.exe') || name.endsWith('.sha256')
  })
  const packageFiles = validFiles.filter((file) => !file.name.toLowerCase().endsWith('.sha256'))
  const versionSources = packageFiles.length ? packageFiles : validFiles
  const detectedVersions = new Map<string, string>()
  for (const file of versionSources) {
    const version = inferVersionFromName(file.name)
    if (version && !detectedVersions.has(version)) detectedVersions.set(version, file.name)
  }

  createReleaseFiles.value = validFiles
  createReleaseVersionSource.value = ''
  if (detectedVersions.size === 1) {
    const [[version, source]] = [...detectedVersions]
    props.form.version = version
    createReleaseVersionSource.value = source
  }

  if (selectedFiles.length !== validFiles.length) {
    createReleaseFileError.value = `${selectedFiles.length - validFiles.length} 个文件不是支持的更新包或 .sha256 文件`
  } else if (detectedVersions.size > 1) {
    createReleaseFileError.value = `检测到多个版本：${[...detectedVersions.keys()].join('、')}，请只选择同一版本的文件`
  } else if (validFiles.length && detectedVersions.size === 0) {
    createReleaseFileError.value = '无法从文件名识别版本号，请手动填写后继续'
  } else {
    createReleaseFileError.value = ''
  }
}

function submitCreateRelease() {
  emit('createRelease', completeCreateRelease)
}

function completeCreateRelease(releaseId: string) {
  const files = createReleaseFiles.value
  artifactUploadRows[releaseId] = [createUploadRow('linux', 'x86_64')]
  batchFileErrors[releaseId] = ''
  batchDragDepths[releaseId] = 0
  batchDragActive[releaseId] = false
  batchInputKeys[releaseId] = 0
  if (files.length) assignBatchFiles(releaseId, files)
  createReleaseFiles.value = []
  createReleaseFileError.value = ''
  createReleaseVersionSource.value = ''
  createReleaseInputKey.value += 1
}

function assignBatchFiles(releaseId: string, files: File[]) {
  const validFiles = files.filter((file) => {
    const name = file.name.toLowerCase()
    return name.endsWith('.bin') || name.endsWith('.exe') || name.endsWith('.sha256')
  })
  const invalidCount = files.length - validFiles.length
  batchFileErrors[releaseId] = invalidCount ? `${invalidCount} 个文件不是支持的更新包或 .sha256 文件` : ''
  const rows = uploadRowsFor(releaseId)
  const packageFiles = validFiles.filter((file) => !file.name.toLowerCase().endsWith('.sha256'))
  const checksumFiles = validFiles.filter((file) => file.name.toLowerCase().endsWith('.sha256'))

  for (const file of packageFiles) {
    const existingRow = rows.find((row) => row.file?.name.toLowerCase() === file.name.toLowerCase())
    const pairedChecksum = checksumFiles.find((checksumFile) => checksumMatchesFile(file, checksumFile))
    const checksumRow = rows.find((row) => row.checksum_file && checksumMatchesFile(file, row.checksum_file))
    const availableRow = rows.find((row) => !row.file && !row.checksum_file)
    const target = inferTargetFromName(file.name)
    const row = existingRow || checksumRow || availableRow || createUploadRow(target.os, target.native_arch || '')
    if (!artifactUploadRows[releaseId].includes(row)) artifactUploadRows[releaseId].push(row)
    row.os = target.os
    row.package_type = 'standalone'
    row.native_arch = target.native_arch
    row.inference = target.inference
    row.file = file
    row.checksum_file = pairedChecksum || row.checksum_file || null
    row.error = !target.os
      ? '无法从文件名识别系统，请手动选择'
      : !target.native_arch
        ? '无法从文件名识别架构，请手动选择'
        : row.checksum_file ? '' : '缺少同名 .sha256 校验文件'
  }

  for (const checksumFile of checksumFiles) {
    const packageName = checksumFile.name.slice(0, -'.sha256'.length).toLowerCase()
    const row = rows.find((item) => item.file?.name.toLowerCase() === packageName)
      || artifactUploadRows[releaseId].find((item) => item.file?.name.toLowerCase() === packageName)
    if (row) {
      row.checksum_file = checksumFile
      if (row.os && row.native_arch) row.error = ''
      continue
    }
    const checksumRow = rows.find((item) => !item.file && !item.checksum_file) || createUploadRow('', '')
    if (!artifactUploadRows[releaseId].includes(checksumRow)) artifactUploadRows[releaseId].push(checksumRow)
    const target = inferTargetFromName(checksumFile.name.slice(0, -'.sha256'.length))
    checksumRow.os = target.os
    checksumRow.native_arch = target.native_arch
    checksumRow.inference = target.inference
    checksumRow.checksum_file = checksumFile
    checksumRow.error = `缺少 ${checksumFile.name.slice(0, -'.sha256'.length)} 可执行文件`
  }
  batchInputKeys[releaseId] += 1
}

function chooseBatchFiles(releaseId: string, event: Event) {
  const input = event.target as HTMLInputElement
  assignBatchFiles(releaseId, Array.from(input.files || []))
}

function batchDragEnter(releaseId: string, event: DragEvent) {
  if (props.operation || !event.dataTransfer?.types.includes('Files')) return
  batchDragDepths[releaseId] += 1
  batchDragActive[releaseId] = true
}

function batchDragLeave(releaseId: string) {
  batchDragDepths[releaseId] = Math.max(0, batchDragDepths[releaseId] - 1)
  if (batchDragDepths[releaseId] === 0) batchDragActive[releaseId] = false
}

function dropBatchFiles(releaseId: string, event: DragEvent) {
  batchDragDepths[releaseId] = 0
  batchDragActive[releaseId] = false
  if (props.operation) return
  assignBatchFiles(releaseId, Array.from(event.dataTransfer?.files || []))
}

function uploadTargetKey(target: AgentArtifactTarget) {
  return `${target.os.trim().toLowerCase()}|${target.package_type}|${target.native_arch.trim()}`
}

function submitArtifacts(release: AgentRelease, onlyRowId?: string) {
  const rows = onlyRowId
    ? uploadRowsFor(release.id).filter((row) => row.id === onlyRowId)
    : uploadRowsFor(release.id)
  const existingTargets = new Set(release.artifacts.map(uploadTargetKey))
  const selectedTargets = new Set<string>()
  const uploads: AgentArtifactUploadItem[] = []
  for (const row of rows) {
    row.error = ''
    if (!row.file) {
      if (row.checksum_file) row.error = '请选择对应的可执行文件'
      continue
    }
    if (!row.checksum_file) {
      row.error = '请选择同名 .sha256 校验文件'
      continue
    }
    if (!row.os) {
      row.error = '请选择目标系统'
      continue
    }
    if (!row.native_arch) {
      row.error = '请选择原生架构'
      continue
    }
    const key = uploadTargetKey(row)
    if (existingTargets.has(key)) {
      row.error = '该版本已包含相同目标的可执行文件'
      continue
    }
    if (selectedTargets.has(key)) {
      row.error = '本次上传中存在重复目标'
      continue
    }
    const expectedExtension = artifactAccept(row)
    if (!row.file.name.toLowerCase().endsWith(expectedExtension)) {
      row.error = `请选择 ${expectedExtension} 可执行文件`
      continue
    }
    if (!checksumMatchesFile(row.file, row.checksum_file)) {
      row.error = '校验文件名必须为可执行文件名加 .sha256'
      continue
    }
    selectedTargets.add(key)
    uploads.push({
      row_id: row.id,
      target: { os: row.os, package_type: row.package_type, native_arch: row.native_arch },
      file: row.file,
      checksum_file: row.checksum_file,
    })
  }
  if (!uploads.length) return
  emit('uploadArtifact', release.id, uploads, (result) => applyUploadResult(release.id, result))
}

function applyUploadResult(releaseId: string, result: AgentArtifactUploadResult) {
  const failed = new Map(result.failures.map((failure) => [failure.row_id, failure.message]))
  const rows = uploadRowsFor(releaseId)
    .filter((row) => !result.succeeded_row_ids.includes(row.id))
  for (const row of rows) {
    if (failed.has(row.id)) row.error = failed.get(row.id) || '上传失败'
  }
  artifactUploadRows[releaseId] = rows.length ? rows : [createUploadRow('linux', 'x86_64')]
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
          <p>为一个版本添加多个系统和架构的可执行文件及同名 .sha256 校验文件。</p>
        </div>
      </div>
      <form class="release-create-form" @submit.prevent="submitCreateRelease">
        <label class="release-create-file-picker">
          <span>更新文件 <i>用于识别版本</i></span>
          <span class="file-button" :class="{ disabled: operation }" :title="createReleaseFiles.map((file) => file.name).join('\n')">
            <Upload :size="17" />
            <span>
              <strong>{{ createReleaseFileSummary }}</strong>
              <small>可同时选择 .bin/.exe 及同名 .sha256</small>
            </span>
            <input
              :key="createReleaseInputKey"
              type="file"
              accept=".bin,.exe,.sha256"
              multiple
              :disabled="Boolean(operation)"
              @change="chooseCreateReleaseFiles"
            />
          </span>
          <small v-if="createReleaseFileError" class="artifact-file-error" role="alert">
            {{ createReleaseFileError }}
          </small>
        </label>
        <label>
          <span>版本号</span>
          <input v-model.trim="form.version" required placeholder="例如：1.4.0" autocomplete="off" />
          <small v-if="createReleaseVersionSource" class="release-version-hint">
            已从 {{ createReleaseVersionSource }} 识别，可直接修改
          </small>
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

          <form v-if="release.status === 'draft'" class="artifact-upload-form" @submit.prevent="submitArtifacts(release)">
            <div class="artifact-upload-toolbar">
              <label
                :class="[
                  'artifact-batch-picker',
                  { dragging: batchDragActive[release.id], disabled: operation },
                ]"
                @dragenter.prevent="batchDragEnter(release.id, $event)"
                @dragleave.prevent="batchDragLeave(release.id)"
                @dragover.prevent
                @drop.prevent="dropBatchFiles(release.id, $event)"
              >
                <Upload :size="17" />
                <span>
                  <strong>{{ batchDragActive[release.id] ? '松开即可添加多个文件' : '批量添加更新包' }}</strong>
                  <small>同时选择更新包及其同名 .sha256 文件</small>
                </span>
                <input
                  :key="batchInputKeys[release.id]"
                  type="file"
                  accept=".bin,.exe,.sha256"
                  multiple
                  :disabled="Boolean(operation)"
                  @change="chooseBatchFiles(release.id, $event)"
                />
              </label>
              <button class="text-button" type="button" :disabled="Boolean(operation)" @click="addUploadRow(release.id)">
                <Plus :size="15" />添加目标
              </button>
              <button
                class="primary-button artifact-upload-button"
                type="submit"
                :disabled="Boolean(operation) || !uploadableRowsFor(release.id).length"
              >
                <LoaderCircle v-if="isBusy(release.id) && operation === 'uploading'" class="spin" :size="15" />
                <Upload v-else :size="15" />上传 {{ uploadableRowsFor(release.id).length }} 个
              </button>
            </div>
            <small v-if="batchFileErrors[release.id]" class="artifact-file-error" role="alert">
              {{ batchFileErrors[release.id] }}
            </small>

            <div class="artifact-upload-rows">
              <div v-for="row in uploadRowsFor(release.id)" :key="row.id" class="artifact-upload-row">
                <label>
                  <span>目标系统</span>
                  <select v-model="row.os" @change="changeArtifactOs(row)">
                    <option v-if="!row.os" value="" disabled>请选择系统</option>
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
                  <select v-model="row.native_arch" required @change="changeArtifactArchitecture(row)">
                    <option v-if="!row.native_arch" value="" disabled>请选择架构</option>
                    <option
                      v-for="architecture in nativeArchitectures(row.os)"
                      :key="architecture"
                      :value="architecture"
                    >
                      {{ architecture }}
                    </option>
                  </select>
                </label>
                <label class="artifact-file-picker">
                  <span>可执行文件</span>
                  <span class="file-button" :title="row.file?.name">
                    <Upload :size="17" />
                    <span>
                      <strong>{{ row.file?.name || '选择单个文件' }}</strong>
                      <small>{{ row.inference === 'needs_target' ? '请确认系统和架构后上传' : row.inference === 'needs_architecture' ? '请确认架构后上传' : `仅支持 ${artifactAccept(row)} 文件` }}</small>
                    </span>
                    <input
                      :key="fileInputKeys[row.id] || 0"
                      type="file"
                      :accept="artifactAccept(row)"
                      :disabled="Boolean(operation)"
                      @change="chooseArtifactFile(row, $event)"
                    />
                  </span>
                </label>
                <label class="artifact-checksum-picker">
                  <span>SHA-256 文件</span>
                  <span class="file-button" :title="row.checksum_file?.name">
                    <FileArchive :size="17" />
                    <span>
                      <strong>{{ row.checksum_file?.name || '选择同名 .sha256' }}</strong>
                      <small>{{ row.file ? `${row.file.name}.sha256` : '需与可执行文件同名' }}</small>
                    </span>
                    <input
                      :key="checksumInputKeys[row.id] || 0"
                      type="file"
                      accept=".sha256"
                      :disabled="Boolean(operation)"
                      @change="chooseChecksumFile(row, $event)"
                    />
                  </span>
                </label>
                <div class="artifact-row-actions">
                  <button
                    v-if="row.file"
                    class="icon-button"
                    type="button"
                    title="单独上传此目标"
                    :aria-label="`上传 ${row.file.name}`"
                    :disabled="Boolean(operation) || !row.os || !row.native_arch || !row.checksum_file"
                    @click="submitArtifacts(release, row.id)"
                  >
                    <Upload :size="15" />
                  </button>
                  <button
                    class="icon-button danger"
                    type="button"
                    title="移除上传目标"
                    :aria-label="`移除上传目标 ${row.file?.name || row.os}`"
                    :disabled="Boolean(operation)"
                    @click="removeUploadRow(release.id, row.id)"
                  >
                    <Trash2 :size="15" />
                  </button>
                </div>
                <small v-if="row.error" class="artifact-file-error" role="alert">{{ row.error }}</small>
              </div>
            </div>
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
