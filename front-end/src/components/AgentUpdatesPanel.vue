<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import {
  ArrowUpCircle,
  Check,
  CircleAlert,
  Clock3,
  Download,
  EllipsisVertical,
  FileArchive,
  LoaderCircle,
  PackageCheck,
  Pause,
  Pencil,
  Play,
  Plus,
  RefreshCw,
  RotateCcw,
  Save,
  Send,
  ShieldAlert,
  Trash2,
  Undo2,
  Upload,
  UserPlus,
  X,
} from '@lucide/vue'
import AgentRolloutSelectorModal from './AgentRolloutSelectorModal.vue'
import WorkspaceDrawer from './WorkspaceDrawer.vue'
import type {
  AgentArtifactTarget,
  AgentArtifactUploadItem,
  AgentArtifactUploadResult,
  AgentArtifactUploadRow,
  AgentRelease,
  AgentReleaseForm,
  AgentRolloutCandidate,
  AgentUpdateAttempt,
  AgentUpdateAttemptStatus,
  Instance,
} from '../types/domain'
import { inferArtifactTarget } from '../utils/agentArtifacts'
import { formatBytes, formatTime } from '../utils/format'

const props = defineProps<{
  instances: Instance[]
  releases: AgentRelease[]
  attempts: AgentUpdateAttempt[]
  form: AgentReleaseForm
  operation: string | null
  busyId: string
  message: string
  rolloutCandidates: Record<string, AgentRolloutCandidate[]>
  rolloutCandidatesLoading: string
}>()

const emit = defineEmits<{
  createRelease: [onCreated: (releaseId: string) => void]
  saveRelease: [releaseId: string, form: AgentReleaseForm]
  uploadArtifact: [releaseId: string, uploads: AgentArtifactUploadItem[], onComplete: (result: AgentArtifactUploadResult) => void]
  deleteArtifact: [releaseId: string, artifactId: string]
  publishRelease: [release: AgentRelease, instanceIds?: string[]]
  deleteRelease: [release: AgentRelease]
  retryAttempt: [attempt: AgentUpdateAttempt]
  loadRolloutCandidates: [releaseId: string]
  addRolloutTargets: [release: AgentRelease, instanceIds: string[]]
  pauseRollout: [release: AgentRelease]
  resumeRollout: [release: AgentRelease]
  promoteRollout: [release: AgentRelease]
  rollbackRelease: [release: AgentRelease]
  rollbackInstance: [release: AgentRelease, attempt: AgentUpdateAttempt]
  reupgradeInstance: [release: AgentRelease, attempt: AgentUpdateAttempt]
}>()

const draftEdits = reactive<Record<string, AgentReleaseForm>>({})
const artifactUploadRows = reactive<Record<string, AgentArtifactUploadRow[]>>({})
const batchFileErrors = reactive<Record<string, string>>({})
const batchDragDepths = reactive<Record<string, number>>({})
const batchDragActive = reactive<Record<string, boolean>>({})
const batchInputKeys = reactive<Record<string, number>>({})
const fileInputKeys = reactive<Record<string, number>>({})
const checksumInputKeys = reactive<Record<string, number>>({})
const fileDragDepths = reactive<Record<string, number>>({})
const fileDragActive = reactive<Record<string, boolean>>({})
const editingReleaseId = ref<string | null>(null)
const rolloutSelector = ref<{ release: AgentRelease; mode: 'publish' | 'add' } | null>(null)
const selectedReleaseId = ref<string | null>(null)
const activeTab = ref<'attempts' | 'overview' | 'artifacts'>('attempts')
const operationDrawer = ref<'create' | 'edit' | 'upload' | null>(null)
const moreMenuOpen = ref(false)
const moreMenuElement = ref<HTMLElement | null>(null)
const moreMenuButton = ref<HTMLButtonElement | null>(null)
const createReleaseFiles = ref<File[]>([])
const createReleaseFileError = ref('')
const createReleaseVersionSource = ref('')
const createReleaseVersionConflict = ref(false)
const createReleaseInputKey = ref(0)
const createReleaseDropKey = 'create-release'
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

const rolloutStatusText: Record<AgentRelease['rollout_state'], string> = {
  draft: '草稿',
  canary_active: '灰度中',
  canary_paused: '灰度已暂停',
  full_active: '全量',
  full_paused: '全量已暂停',
  rollback_active: '回滚中',
  rolled_back: '已回滚',
  rollback_partial: '部分回滚',
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
  cancelled: '已取消',
}

const terminalAttemptStatuses = new Set<AgentUpdateAttemptStatus>([
  'succeeded',
  'rollback_succeeded',
  'failed',
  'cancelled',
])

const publishedCount = computed(() => props.releases.filter((release) => release.status === 'published').length)
const instancesById = computed(() => new Map(props.instances.map((instance) => [instance.id, instance])))
const selectedRelease = computed(() => (
  props.releases.find((release) => release.id === selectedReleaseId.value) || null
))
const operationDrawerTitle = computed(() => {
  if (operationDrawer.value === 'create') return '新建 Agent 版本'
  if (operationDrawer.value === 'edit') return `编辑 Agent ${selectedRelease.value?.version || ''}`
  return `添加 Agent ${selectedRelease.value?.version || ''} 更新包`
})
const operationDrawerDescription = computed(() => {
  if (operationDrawer.value === 'create') return '创建草稿，可同时选择更新包以继续上传。'
  if (operationDrawer.value === 'edit') return '修改版本号和发布说明。'
  return '为不同系统与原生架构上传可执行文件及其 SHA-256 校验文件。'
})
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
    if (!selectedReleaseId.value || !releases.some((release) => release.id === selectedReleaseId.value)) {
      selectedReleaseId.value = releases[0]?.id || null
      if (operationDrawer.value !== 'create') operationDrawer.value = null
    }
  },
  { immediate: true },
)

watch(
  () => props.operation,
  (operation, previousOperation) => {
    if (!operation && previousOperation === 'saving' && props.message && editingReleaseId.value) {
      editingReleaseId.value = null
      operationDrawer.value = null
    }
  },
)

watch(selectedReleaseId, () => {
  moreMenuOpen.value = false
})

onMounted(() => {
  document.addEventListener('pointerdown', handleOutsidePointerDown)
  document.addEventListener('keydown', handleMenuKeydown)
})

onBeforeUnmount(() => {
  document.removeEventListener('pointerdown', handleOutsidePointerDown)
  document.removeEventListener('keydown', handleMenuKeydown)
})

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

function artifactDownloadUrl(releaseId: string, artifactId: string) {
  return `/api/admin/agent-releases/${encodeURIComponent(releaseId)}/artifacts/${encodeURIComponent(artifactId)}/download`
}

function downloadArtifact(releaseId: string, artifact: AgentRelease['artifacts'][number]) {
  const anchor = document.createElement('a')
  anchor.href = artifactDownloadUrl(releaseId, artifact.id)
  anchor.download = artifact.file_name
  document.body.appendChild(anchor)
  anchor.click()
  anchor.remove()
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
  row.error = row.file && !row.checksum_file ? '请选择同名 .sha256 校验文件' : ''
}

function uploadRowTargetError(row: AgentArtifactUploadRow) {
  if (row.inference === 'needs_target' && !row.os) return '无法从文件名识别系统，请手动选择'
  if (row.inference === 'needs_architecture' && !row.native_arch) return '无法从文件名识别架构，请手动选择'
  return ''
}

function chooseArtifactFile(row: AgentArtifactUploadRow, event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (file) setUploadRowFile(row, file, true)
}

function setUploadRowFile(row: AgentArtifactUploadRow, file: File, inferTarget = false) {
  const inferredTarget = inferTarget ? inferArtifactTarget(file.name) : null
  const targetOs = inferredTarget?.os || row.os
  const expectedExtension = targetOs === 'windows' ? '.exe' : '.bin'
  if (!file.name.toLowerCase().endsWith(expectedExtension)) {
    if (!inferTarget) {
      row.file = null
      row.checksum_file = null
      checksumInputKeys[row.id] = (checksumInputKeys[row.id] || 0) + 1
    }
    row.error = `请选择 ${expectedExtension} 可执行文件`
    fileInputKeys[row.id] = (fileInputKeys[row.id] || 0) + 1
    return
  }
  if (inferredTarget?.os) {
    row.os = inferredTarget.os
    row.native_arch = inferredTarget.native_arch
    row.inference = inferredTarget.inference
  } else {
    row.inference = 'manual'
  }
  row.file = file
  if (row.checksum_file && !checksumMatchesFile(file, row.checksum_file)) {
    row.checksum_file = null
    checksumInputKeys[row.id] = (checksumInputKeys[row.id] || 0) + 1
  }
  row.error = uploadRowTargetError(row)
    || (row.checksum_file ? '' : '请选择同名 .sha256 校验文件')
}

function chooseChecksumFile(row: AgentArtifactUploadRow, event: Event) {
  const input = event.target as HTMLInputElement
  const file = input.files?.[0]
  if (file) setUploadRowChecksumFile(row, file)
}

function setUploadRowChecksumFile(row: AgentArtifactUploadRow, file: File) {
  if (!file.name.toLowerCase().endsWith('.sha256')) {
    row.error = '请选择 .sha256 校验文件'
    checksumInputKeys[row.id] = (checksumInputKeys[row.id] || 0) + 1
    return
  }
  if (row.file && !checksumMatchesFile(row.file, file)) {
    row.error = '校验文件名必须为可执行文件名加 .sha256'
    checksumInputKeys[row.id] = (checksumInputKeys[row.id] || 0) + 1
    return
  }
  row.checksum_file = file
  row.error = uploadRowTargetError(row) || (row.file ? '' : '请先选择对应的可执行文件')
}

function checksumMatchesFile(file: File, checksumFile: File) {
  return checksumFile.name.toLowerCase() === `${file.name.toLowerCase()}.sha256`
}

function addUploadRow(releaseId: string, os = 'linux', nativeArch = 'x86_64') {
  artifactUploadRows[releaseId].push(createUploadRow(os, nativeArch))
}

function removeUploadRow(releaseId: string, rowId: string) {
  const rows = artifactUploadRows[releaseId]
  clearUploadRowDragState(rowId)
  if (rows.length <= 1) {
    rows[0].file = null
    rows[0].checksum_file = null
    rows[0].error = ''
    rows[0].inference = 'manual'
    return
  }
  artifactUploadRows[releaseId] = rows.filter((row) => row.id !== rowId)
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
  setCreateReleaseFiles(Array.from(input.files || []))
}

function setCreateReleaseFiles(selectedFiles: File[]) {
  const validFiles = selectedFiles.filter((file) => {
    const name = file.name.toLowerCase()
    return name.endsWith('.bin') || name.endsWith('.exe') || name.endsWith('.sha256')
  })
  const packageFiles = validFiles.filter((file) => !file.name.toLowerCase().endsWith('.sha256'))
  const versionSources = packageFiles.length ? packageFiles : validFiles
  const detectedVersions = new Map<string, string>()
  const unrecognizedVersionFiles: string[] = []
  for (const file of versionSources) {
    const version = inferVersionFromName(file.name)
    if (version) {
      if (!detectedVersions.has(version)) detectedVersions.set(version, file.name)
    } else {
      unrecognizedVersionFiles.push(file.name)
    }
  }

  createReleaseFiles.value = validFiles
  createReleaseVersionSource.value = ''
  createReleaseVersionConflict.value = detectedVersions.size > 1
    || (detectedVersions.size > 0 && unrecognizedVersionFiles.length > 0)
  if (detectedVersions.size === 1 && !unrecognizedVersionFiles.length) {
    const [[version, source]] = [...detectedVersions]
    props.form.version = version
    createReleaseVersionSource.value = source
  } else {
    props.form.version = ''
  }

  if (selectedFiles.length !== validFiles.length) {
    createReleaseFileError.value = `${selectedFiles.length - validFiles.length} 个文件不是支持的更新包或 .sha256 文件`
  } else if (detectedVersions.size > 1) {
    createReleaseFileError.value = `检测到多个版本：${[...detectedVersions.keys()].join('、')}，请只选择同一版本的文件`
  } else if (detectedVersions.size > 0 && unrecognizedVersionFiles.length) {
    createReleaseFileError.value = `以下文件无法识别版本：${unrecognizedVersionFiles.join('、')}，请只选择能确认属于同一版本的文件`
  } else if (validFiles.length && detectedVersions.size === 0) {
    createReleaseFileError.value = '无法从文件名识别版本号，请手动填写后继续'
  } else {
    createReleaseFileError.value = ''
  }
}

function submitCreateRelease() {
  if (createReleaseVersionConflict.value) return
  if (!props.form.version.trim()) {
    createReleaseFileError.value = '请输入 Agent 版本号'
    return
  }
  emit('createRelease', completeCreateRelease)
}

function completeCreateRelease(releaseId: string) {
  const files = createReleaseFiles.value
  selectedReleaseId.value = releaseId
  activeTab.value = files.length ? 'artifacts' : 'attempts'
  artifactUploadRows[releaseId] = [createUploadRow('linux', 'x86_64')]
  batchFileErrors[releaseId] = ''
  batchDragDepths[releaseId] = 0
  batchDragActive[releaseId] = false
  batchInputKeys[releaseId] = 0
  if (files.length) assignBatchFiles(releaseId, files)
  createReleaseFiles.value = []
  createReleaseFileError.value = ''
  createReleaseVersionSource.value = ''
  createReleaseVersionConflict.value = false
  createReleaseInputKey.value += 1
  operationDrawer.value = files.length ? 'upload' : null
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
    const target = inferArtifactTarget(file.name)
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
    const target = inferArtifactTarget(checksumFile.name)
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
  if (props.operation || !hasDraggedFiles(event)) return
  event.preventDefault()
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

function uploadRowDropKey(rowId: string, kind: 'executable' | 'checksum') {
  return `${rowId}:${kind}`
}

function hasDraggedFiles(event: DragEvent) {
  return Array.from(event.dataTransfer?.types || []).includes('Files')
}

function fileDragOver(event: DragEvent) {
  const transfer = event.dataTransfer
  if (props.operation || !hasDraggedFiles(event)) {
    if (transfer) transfer.dropEffect = 'none'
    return
  }
  event.preventDefault()
  if (transfer) transfer.dropEffect = 'copy'
}

function isFileDragActive(key: string) {
  return Boolean(fileDragActive[key])
}

function fileDragEnter(key: string, event: DragEvent) {
  if (props.operation || !hasDraggedFiles(event)) return
  event.preventDefault()
  fileDragDepths[key] = (fileDragDepths[key] || 0) + 1
  fileDragActive[key] = true
}

function fileDragLeave(key: string) {
  fileDragDepths[key] = Math.max(0, (fileDragDepths[key] || 0) - 1)
  if (fileDragDepths[key] === 0) fileDragActive[key] = false
}

function resetFileDrag(key: string) {
  fileDragDepths[key] = 0
  fileDragActive[key] = false
}

function clearUploadRowDragState(rowId: string) {
  for (const kind of ['executable', 'checksum'] as const) {
    const key = uploadRowDropKey(rowId, kind)
    delete fileDragDepths[key]
    delete fileDragActive[key]
  }
}

function dropCreateReleaseFiles(event: DragEvent) {
  resetFileDrag(createReleaseDropKey)
  if (props.operation) return
  const files = Array.from(event.dataTransfer?.files || [])
  if (files.length) setCreateReleaseFiles(files)
}

function dropArtifactFile(row: AgentArtifactUploadRow, event: DragEvent) {
  resetFileDrag(uploadRowDropKey(row.id, 'executable'))
  if (props.operation) return
  const files = Array.from(event.dataTransfer?.files || [])
  const executableFiles = files.filter((file) => /\.(?:bin|exe)$/i.test(file.name))
  if (executableFiles.length !== 1) {
    row.error = executableFiles.length
      ? '单个目标一次只能拖入一个可执行文件，多个目标请使用批量添加'
      : '请拖入 .bin 或 .exe 可执行文件'
    return
  }
  const [file] = executableFiles
  setUploadRowFile(row, file, true)
  const checksum = files.find((candidate) => checksumMatchesFile(file, candidate))
  if (row.file === file && checksum) setUploadRowChecksumFile(row, checksum)
}

function dropChecksumFile(row: AgentArtifactUploadRow, event: DragEvent) {
  resetFileDrag(uploadRowDropKey(row.id, 'checksum'))
  if (props.operation) return
  const checksumFiles = Array.from(event.dataTransfer?.files || [])
    .filter((file) => file.name.toLowerCase().endsWith('.sha256'))
  if (checksumFiles.length !== 1) {
    row.error = checksumFiles.length
      ? '一次只能拖入一个 SHA-256 校验文件'
      : '请拖入 .sha256 校验文件'
    return
  }
  setUploadRowChecksumFile(row, checksumFiles[0])
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
  for (const rowId of result.succeeded_row_ids) clearUploadRowDragState(rowId)
  const rows = uploadRowsFor(releaseId)
    .filter((row) => !result.succeeded_row_ids.includes(row.id))
  for (const row of rows) {
    if (failed.has(row.id)) row.error = failed.get(row.id) || '上传失败'
  }
  const pendingRows = rows.filter((row) => row.file || row.checksum_file || row.error)
  artifactUploadRows[releaseId] = pendingRows.length ? pendingRows : [createUploadRow('linux', 'x86_64')]
  const hasBatchFailure = Boolean(batchFileErrors[releaseId])
  if (pendingRows.length === 0 && result.failures.length === 0 && !hasBatchFailure) operationDrawer.value = null
}

function editRelease(release: AgentRelease) {
  draftEdits[release.id] = { version: release.version, notes: release.notes }
  selectedReleaseId.value = release.id
  editingReleaseId.value = release.id
  operationDrawer.value = 'edit'
  moreMenuOpen.value = false
}

function cancelEdit(releaseId: string) {
  const release = props.releases.find((item) => item.id === releaseId)
  if (release) draftEdits[releaseId] = { version: release.version, notes: release.notes }
  editingReleaseId.value = null
  operationDrawer.value = null
}

function saveRelease(releaseId: string) {
  emit('saveRelease', releaseId, { ...draftEdits[releaseId] })
}

function attemptsFor(release: AgentRelease) {
  return release.attempts.length
    ? release.attempts
    : props.attempts.filter((attempt) => attempt.release_id === release.id)
}

function attemptStats(release: AgentRelease) {
  const attempts = attemptsFor(release)
  return {
    active: attempts.filter((attempt) => !terminalAttemptStatuses.has(attempt.status)).length,
    upgradeSucceeded: attempts.filter(
      (attempt) => attempt.operation === 'upgrade' && attempt.status === 'succeeded',
    ).length,
    rollbackSucceeded: attempts.filter(
      (attempt) => attempt.operation === 'rollback' && attempt.status === 'rollback_succeeded',
    ).length,
    failed: attempts.filter((attempt) => attempt.status === 'failed').length,
  }
}

function activeAttemptCount(release: AgentRelease) {
  return attemptsFor(release).filter((attempt) => !terminalAttemptStatuses.has(attempt.status)).length
}

function canDeleteRelease(release: AgentRelease) {
  return activeAttemptCount(release) === 0
    && !attemptsFor(release).some((attempt) => attempt.operation === 'rollback')
}

function deleteReleaseTitle(release: AgentRelease) {
  const activeCount = activeAttemptCount(release)
  if (activeCount > 0) return `仍有 ${activeCount} 个实例更新未结束，暂不能删除`
  const rollbackRecords = attemptsFor(release).filter((attempt) => attempt.operation === 'rollback').length
  if (rollbackRecords > 0) return `仍有 ${rollbackRecords} 条回滚记录依赖此版本，暂不能删除`
  return release.status === 'published' ? '永久删除已发布版本' : '删除草稿'
}

function deleteReleaseLabel(release: AgentRelease) {
  return `${deleteReleaseTitle(release)}：Agent ${release.version}`
}

function draftArtifactCount(release: AgentRelease) {
  return release.artifacts.filter((artifact) => artifact.status === 'draft').length
}

function attemptInstanceName(attempt: AgentUpdateAttempt) {
  const instance = instancesById.value.get(attempt.instance_id)
  return instance?.name || instance?.hostname || attempt.instance_id
}

function openRolloutSelector(release: AgentRelease, mode: 'publish' | 'add') {
  rolloutSelector.value = { release, mode }
  emit('loadRolloutCandidates', release.id)
}

function confirmRolloutTargets(instanceIds: string[]) {
  const selector = rolloutSelector.value
  if (!selector) return
  rolloutSelector.value = null
  if (selector.mode === 'publish') {
    emit('publishRelease', selector.release, instanceIds)
  } else {
    emit('addRolloutTargets', selector.release, instanceIds)
  }
}

function isRolloutActive(release: AgentRelease) {
  return release.rollout_state === 'canary_active' || release.rollout_state === 'full_active'
}

function isRolloutPaused(release: AgentRelease) {
  return release.rollout_state === 'canary_paused' || release.rollout_state === 'full_paused'
}

function canRollbackRelease(release: AgentRelease) {
  return [
    'canary_active',
    'canary_paused',
    'full_active',
    'full_paused',
  ].includes(release.rollout_state)
}

function canRollbackInstance(release: AgentRelease, attempt: AgentUpdateAttempt) {
  return attempt.operation === 'upgrade'
    && attempt.status === 'succeeded'
    && ['canary_active', 'canary_paused', 'full_active', 'full_paused'].includes(release.rollout_state)
    && !attemptsFor(release).some(
      (candidate) => candidate.operation === 'rollback' && candidate.parent_attempt_id === attempt.id,
  )
}

function canRetryAttempt(release: AgentRelease, attempt: AgentUpdateAttempt) {
  if (attempt.operation === 'upgrade') {
    return ['failed', 'rollback_succeeded'].includes(attempt.status)
      && ['canary_active', 'canary_paused', 'full_active', 'full_paused'].includes(release.rollout_state)
  }
  return attempt.status === 'failed' && [
    'canary_active',
    'canary_paused',
    'full_active',
    'full_paused',
    'rollback_active',
    'rollback_partial',
  ].includes(release.rollout_state)
}

function canReupgradeInstance(release: AgentRelease, attempt: AgentUpdateAttempt) {
  return attempt.operation === 'rollback'
    && attempt.status === 'rollback_succeeded'
    && ['canary_active', 'canary_paused', 'full_active', 'full_paused'].includes(release.rollout_state)
    && !attemptsFor(release).some(
      (candidate) => candidate.instance_id === attempt.instance_id
        && candidate.operation === 'upgrade'
        && candidate.created_at > attempt.created_at,
    )
}

function canPublishArtifacts(release: AgentRelease) {
  return release.status === 'draft'
    || !['rollback_active', 'rolled_back', 'rollback_partial'].includes(release.rollout_state)
}

function isCanaryRollout(release: AgentRelease) {
  return release.rollout_state === 'canary_active' || release.rollout_state === 'canary_paused'
}

function isBusy(id: string) {
  return Boolean(props.operation) && props.busyId === id
}

function selectRelease(releaseId: string) {
  selectedReleaseId.value = releaseId
}

function openCreateDrawer() {
  operationDrawer.value = 'create'
  moreMenuOpen.value = false
}

function openUploadDrawer(release: AgentRelease) {
  selectedReleaseId.value = release.id
  activeTab.value = 'artifacts'
  operationDrawer.value = 'upload'
  moreMenuOpen.value = false
}

function closeOperationDrawer() {
  if (props.operation) return
  if (operationDrawer.value === 'edit' && editingReleaseId.value) {
    const release = props.releases.find((item) => item.id === editingReleaseId.value)
    if (release) draftEdits[release.id] = { version: release.version, notes: release.notes }
    editingReleaseId.value = null
  }
  operationDrawer.value = null
}

async function toggleMoreMenu() {
  moreMenuOpen.value = !moreMenuOpen.value
  if (!moreMenuOpen.value) return
  await nextTick()
  menuItems()[0]?.focus()
}

function menuItems() {
  return Array.from(
    moreMenuElement.value?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not([disabled])') || [],
  )
}

function closeMoreMenu(restoreFocus = false) {
  if (!moreMenuOpen.value) return
  moreMenuOpen.value = false
  if (restoreFocus) void nextTick(() => moreMenuButton.value?.focus())
}

function handleMoreMenuKeydown(event: KeyboardEvent) {
  const items = menuItems()
  if (!items.length) return
  const currentIndex = items.indexOf(document.activeElement as HTMLButtonElement)
  let nextIndex: number | null = null
  if (event.key === 'ArrowDown') nextIndex = currentIndex < 0 ? 0 : (currentIndex + 1) % items.length
  if (event.key === 'ArrowUp') nextIndex = currentIndex < 0 ? items.length - 1 : (currentIndex - 1 + items.length) % items.length
  if (event.key === 'Home') nextIndex = 0
  if (event.key === 'End') nextIndex = items.length - 1
  if (nextIndex === null) return
  event.preventDefault()
  items[nextIndex]?.focus()
}

function handleMoreMenuFocusout(event: FocusEvent) {
  if (event.relatedTarget instanceof Node && moreMenuElement.value?.contains(event.relatedTarget)) return
  closeMoreMenu()
}

function handleTabKeydown(event: KeyboardEvent) {
  const tabs = ['attempts', 'overview', 'artifacts'] as const
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

function handleOutsidePointerDown(event: PointerEvent) {
  if (moreMenuOpen.value && !moreMenuElement.value?.contains(event.target as Node)) {
    closeMoreMenu()
  }
}

function handleMenuKeydown(event: KeyboardEvent) {
  if (event.key !== 'Escape' || !moreMenuOpen.value) return
  event.preventDefault()
  closeMoreMenu(true)
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
      <div class="updates-header-actions">
        <span class="page-count">{{ publishedCount }} 个已发布</span>
        <button class="primary-button" type="button" :disabled="Boolean(operation)" @click="openCreateDrawer">
          <Plus :size="16" />新建版本
        </button>
      </div>
    </header>

    <Transition name="notice">
      <p v-if="message" class="update-feedback" role="status" aria-live="polite">
        <Check :size="15" />{{ message }}
      </p>
    </Transition>

    <section class="updates-workspace" aria-label="Agent 版本工作台">
      <aside class="release-rail" aria-labelledby="agent-release-list-title">
        <div class="release-rail-heading">
          <div>
            <span class="section-kicker">Release history</span>
            <h3 id="agent-release-list-title">版本</h3>
          </div>
          <span>{{ releases.length }}</span>
        </div>

        <div v-if="releases.length" class="release-rail-list">
          <button
            v-for="release in releases"
            :key="release.id"
            type="button"
            :class="['release-rail-item', { selected: selectedReleaseId === release.id }]"
            :aria-current="selectedReleaseId === release.id ? 'true' : undefined"
            @click="selectRelease(release.id)"
          >
            <span class="release-rail-title">
              <strong>Agent {{ release.version }}</strong>
              <span :class="['release-status', release.status]">{{ releaseStatusText[release.status] }}</span>
            </span>
            <span class="release-rail-meta">
              <span v-if="release.status === 'published'" :class="['rollout-status', release.rollout_state]">
                {{ rolloutStatusText[release.rollout_state] }}
              </span>
              <time>{{ formatTime(release.rollout_updated_at || release.published_at || release.created_at) }}</time>
            </span>
            <span class="release-rail-stats" aria-label="实例更新统计">
              <span class="success">成功 {{ attemptStats(release).upgradeSucceeded + attemptStats(release).rollbackSucceeded }}</span>
              <span class="active">处理中 {{ attemptStats(release).active }}</span>
              <span :class="{ danger: attemptStats(release).failed > 0 }">失败 {{ attemptStats(release).failed }}</span>
            </span>
          </button>
        </div>

        <div v-else class="page-empty update-empty">
          <span><PackageCheck :size="24" /></span>
          <strong>暂无更新版本</strong>
          <p>创建草稿后即可上传更新包。</p>
          <button class="primary-button" type="button" @click="openCreateDrawer">
            <Plus :size="15" />新建版本
          </button>
        </div>
      </aside>

      <article v-if="selectedRelease" class="release-detail">
        <header class="release-detail-header">
          <div class="release-detail-identity">
            <div class="release-detail-title">
              <h3>Agent {{ selectedRelease.version }}</h3>
              <span :class="['release-status', selectedRelease.status]">{{ releaseStatusText[selectedRelease.status] }}</span>
              <span
                v-if="selectedRelease.status === 'published'"
                :class="['rollout-status', selectedRelease.rollout_state]"
              >{{ rolloutStatusText[selectedRelease.rollout_state] }}</span>
            </div>
            <p :class="['release-notes', { empty: !selectedRelease.notes }]">
              {{ selectedRelease.notes || '未填写发布说明' }}
            </p>
          </div>

          <div class="release-detail-actions">
            <button
              v-if="draftArtifactCount(selectedRelease) > 0"
              class="primary-button release-publish-button"
              type="button"
              :disabled="Boolean(operation) || !canPublishArtifacts(selectedRelease)"
              @click="selectedRelease.status === 'draft' ? openRolloutSelector(selectedRelease, 'publish') : $emit('publishRelease', selectedRelease)"
            >
              <LoaderCircle v-if="isBusy(selectedRelease.id) && operation === 'publishing'" class="spin" :size="15" />
              <Send v-else :size="15" />{{ selectedRelease.status === 'published' ? '发布新增包' : '发布' }}
            </button>
            <button
              v-else-if="isRolloutActive(selectedRelease)"
              class="primary-button"
              type="button"
              :disabled="Boolean(operation)"
              @click="$emit('pauseRollout', selectedRelease)"
            >
              <LoaderCircle v-if="isBusy(selectedRelease.id) && operation === 'pausing'" class="spin" :size="15" />
              <Pause v-else :size="15" />暂停发布
            </button>
            <button
              v-else-if="isRolloutPaused(selectedRelease)"
              class="primary-button"
              type="button"
              :disabled="Boolean(operation)"
              @click="$emit('resumeRollout', selectedRelease)"
            >
              <LoaderCircle v-if="isBusy(selectedRelease.id) && operation === 'resuming'" class="spin" :size="15" />
              <Play v-else :size="15" />恢复发布
            </button>

            <div ref="moreMenuElement" class="release-more" @focusout="handleMoreMenuFocusout">
              <button
                ref="moreMenuButton"
                class="icon-button"
                type="button"
                title="更多版本操作"
                aria-label="更多版本操作"
                aria-haspopup="menu"
                :aria-expanded="moreMenuOpen"
                @click="toggleMoreMenu"
              >
                <EllipsisVertical :size="17" />
              </button>
              <div v-if="moreMenuOpen" class="release-more-menu" role="menu" @keydown="handleMoreMenuKeydown">
                <button
                  v-if="selectedRelease.status === 'draft'"
                  type="button"
                  role="menuitem"
                  :disabled="Boolean(operation)"
                  @click="editRelease(selectedRelease)"
                ><Pencil :size="15" />编辑草稿</button>
                <button
                  v-if="draftArtifactCount(selectedRelease) > 0 && isRolloutActive(selectedRelease)"
                  type="button"
                  role="menuitem"
                  :disabled="Boolean(operation)"
                  @click="moreMenuOpen = false; $emit('pauseRollout', selectedRelease)"
                ><Pause :size="15" />暂停发布</button>
                <button
                  v-if="draftArtifactCount(selectedRelease) > 0 && isRolloutPaused(selectedRelease)"
                  type="button"
                  role="menuitem"
                  :disabled="Boolean(operation)"
                  @click="moreMenuOpen = false; $emit('resumeRollout', selectedRelease)"
                ><Play :size="15" />恢复发布</button>
                <button
                  v-if="isCanaryRollout(selectedRelease)"
                  type="button"
                  role="menuitem"
                  :disabled="Boolean(operation)"
                  @click="moreMenuOpen = false; openRolloutSelector(selectedRelease, 'add')"
                ><UserPlus :size="15" />添加灰度批次</button>
                <button
                  v-if="isCanaryRollout(selectedRelease)"
                  type="button"
                  role="menuitem"
                  :disabled="Boolean(operation)"
                  @click="moreMenuOpen = false; $emit('promoteRollout', selectedRelease)"
                ><ArrowUpCircle :size="15" />晋级全量</button>
                <button
                  v-if="canRollbackRelease(selectedRelease)"
                  class="danger"
                  type="button"
                  role="menuitem"
                  :disabled="Boolean(operation)"
                  @click="moreMenuOpen = false; $emit('rollbackRelease', selectedRelease)"
                ><Undo2 :size="15" />批量回滚</button>
                <button
                  class="danger"
                  type="button"
                  role="menuitem"
                  :title="deleteReleaseTitle(selectedRelease)"
                  :aria-label="deleteReleaseLabel(selectedRelease)"
                  :disabled="Boolean(operation) || !canDeleteRelease(selectedRelease)"
                  @click="moreMenuOpen = false; $emit('deleteRelease', selectedRelease)"
                >
                  <LoaderCircle v-if="isBusy(selectedRelease.id) && operation === 'deleting'" class="spin" :size="15" />
                  <Trash2 v-else :size="15" />删除版本
                </button>
              </div>
            </div>
          </div>
        </header>

        <div class="release-tabs" role="tablist" aria-label="版本详情视图" @keydown="handleTabKeydown">
          <button
            id="release-tab-attempts"
            type="button"
            role="tab"
            :aria-selected="activeTab === 'attempts'"
            aria-controls="release-panel-attempts"
            :tabindex="activeTab === 'attempts' ? 0 : -1"
            :class="{ active: activeTab === 'attempts' }"
            @click="activeTab = 'attempts'"
          >实例更新 <span>{{ attemptsFor(selectedRelease).length }}</span></button>
          <button
            id="release-tab-overview"
            type="button"
            role="tab"
            :aria-selected="activeTab === 'overview'"
            aria-controls="release-panel-overview"
            :tabindex="activeTab === 'overview' ? 0 : -1"
            :class="{ active: activeTab === 'overview' }"
            @click="activeTab = 'overview'"
          >版本概览</button>
          <button
            id="release-tab-artifacts"
            type="button"
            role="tab"
            :aria-selected="activeTab === 'artifacts'"
            aria-controls="release-panel-artifacts"
            :tabindex="activeTab === 'artifacts' ? 0 : -1"
            :class="{ active: activeTab === 'artifacts' }"
            @click="activeTab = 'artifacts'"
          >更新包 <span>{{ selectedRelease.artifacts.length }}</span></button>
        </div>

        <section
          v-if="activeTab === 'attempts'"
          id="release-panel-attempts"
          class="release-tab-panel release-attempts"
          role="tabpanel"
          aria-labelledby="release-tab-attempts"
        >
          <div v-if="attemptsFor(selectedRelease).length === 0" class="release-empty-row release-tab-empty">
            <Clock3 :size="18" />尚无实例更新记录
          </div>
          <div v-else class="update-attempts-table">
            <div class="update-attempts-head">
              <span>实例</span><span>操作</span><span>版本</span><span>状态</span><span>说明</span><span>更新时间</span><span></span>
            </div>
            <div class="update-attempts-body">
              <article v-for="attempt in attemptsFor(selectedRelease)" :key="attempt.id" class="update-attempt-row">
                <div class="attempt-instance">
                  <strong :title="attemptInstanceName(attempt)">{{ attemptInstanceName(attempt) }}</strong>
                  <small :title="attempt.instance_id">{{ attempt.instance_id.slice(0, 12) }}</small>
                </div>
                <span :class="['attempt-operation', attempt.operation]">
                  <ArrowUpCircle v-if="attempt.operation === 'upgrade'" :size="13" />
                  <Undo2 v-else :size="13" />
                  {{ attempt.operation === 'upgrade' ? '升级' : '回滚' }}
                </span>
                <span class="attempt-versions">{{ attempt.from_version }} -&gt; {{ attempt.target_version }}</span>
                <span :class="['attempt-status', attempt.status]">{{ attemptStatusText[attempt.status] }}</span>
                <span class="attempt-message" :title="attempt.message || '暂无补充说明'">{{ attempt.message || '—' }}</span>
                <time>{{ formatTime(attempt.updated_at) }}</time>
                <div class="attempt-actions">
                  <button
                    v-if="canRetryAttempt(selectedRelease, attempt)"
                    class="icon-button"
                    type="button"
                    title="重试此任务"
                    :aria-label="`重试实例 ${attempt.instance_id} 的${attempt.operation === 'upgrade' ? '升级' : '回滚'}任务`"
                    :disabled="Boolean(operation)"
                    @click="$emit('retryAttempt', attempt)"
                  >
                    <LoaderCircle v-if="isBusy(attempt.id) && operation === 'retrying'" class="spin" :size="15" />
                    <RotateCcw v-else :size="15" />
                  </button>
                  <button
                    v-else-if="canRollbackInstance(selectedRelease, attempt)"
                    class="icon-button danger"
                    type="button"
                    title="回滚此实例"
                    :aria-label="`将实例 ${attempt.instance_id} 回滚到 ${attempt.from_version}`"
                    :disabled="Boolean(operation)"
                    @click="$emit('rollbackInstance', selectedRelease, attempt)"
                  >
                    <LoaderCircle v-if="isBusy(attempt.id) && operation === 'rolling_back'" class="spin" :size="15" />
                    <Undo2 v-else :size="15" />
                  </button>
                  <button
                    v-else-if="canReupgradeInstance(selectedRelease, attempt)"
                    class="icon-button"
                    type="button"
                    title="重新升级此实例"
                    :aria-label="`将实例 ${attempt.instance_id} 重新升级到 ${selectedRelease.version}`"
                    :disabled="Boolean(operation)"
                    @click="$emit('reupgradeInstance', selectedRelease, attempt)"
                  >
                    <LoaderCircle v-if="isBusy(attempt.id) && operation === 'reupgrading'" class="spin" :size="15" />
                    <RefreshCw v-else :size="15" />
                  </button>
                </div>
              </article>
            </div>
          </div>
        </section>

        <section
          v-else-if="activeTab === 'overview'"
          id="release-panel-overview"
          class="release-tab-panel release-overview"
          role="tabpanel"
          aria-labelledby="release-tab-overview"
        >
          <div class="overview-summary">
            <div>
              <span>发布策略</span>
              <strong>{{ selectedRelease.status === 'draft' ? '尚未发布' : (isCanaryRollout(selectedRelease) ? '灰度发布' : '全量发布') }}</strong>
              <small>{{ rolloutStatusText[selectedRelease.rollout_state] }}</small>
            </div>
            <div>
              <span>平台覆盖</span>
              <strong>{{ selectedRelease.coverage.covered_instances }} / {{ selectedRelease.coverage.eligible_instances }}</strong>
              <small>{{ selectedRelease.coverage.missing_artifact_instances }} 个实例缺少更新包</small>
            </div>
            <div>
              <span>回滚能力</span>
              <strong>{{ selectedRelease.rollback_coverage.rollback_supported }} / {{ selectedRelease.rollback_coverage.succeeded_upgrades }}</strong>
              <small>{{ selectedRelease.rollback_coverage.unavailable }} 个实例不可回滚</small>
            </div>
            <div>
              <span>最近更新</span>
              <strong>{{ formatTime(selectedRelease.rollout_updated_at || selectedRelease.published_at || selectedRelease.created_at) }}</strong>
              <small>{{ selectedRelease.status === 'published' ? '发布状态更新时间' : '草稿创建时间' }}</small>
            </div>
          </div>

          <div class="release-coverage" :aria-label="`Agent ${selectedRelease.version} 覆盖情况`">
            <span><strong>{{ selectedRelease.coverage.selected_instances }}</strong> 显式目标</span>
            <span><strong>{{ attemptStats(selectedRelease).upgradeSucceeded }}</strong> 升级成功</span>
            <span><strong>{{ attemptStats(selectedRelease).rollbackSucceeded }}</strong> 回滚成功</span>
            <span><strong>{{ attemptStats(selectedRelease).active }}</strong> 处理中</span>
            <span :class="{ warning: attemptStats(selectedRelease).failed > 0 }"><strong>{{ attemptStats(selectedRelease).failed }}</strong> 失败</span>
            <span v-if="selectedRelease.rollback_coverage.unavailable > 0" class="warning">
              <ShieldAlert :size="14" />{{ selectedRelease.rollback_coverage.unavailable }} 不可回滚
            </span>
            <span :class="{ warning: selectedRelease.coverage.missing_artifact_instances > 0 }">
              <CircleAlert :size="14" />{{ selectedRelease.coverage.missing_artifact_instances }} 缺少可执行文件
            </span>
            <span :class="{ warning: selectedRelease.coverage.unprivileged_instances > 0 }">
              <ShieldAlert :size="14" />{{ selectedRelease.coverage.unprivileged_instances }} 权限不足
            </span>
          </div>

          <div class="overview-notes">
            <span>发布说明</span>
            <p>{{ selectedRelease.notes || '未填写发布说明。' }}</p>
            <div>
              <time>创建于 {{ formatTime(selectedRelease.created_at) }}</time>
              <time v-if="selectedRelease.published_at">发布于 {{ formatTime(selectedRelease.published_at) }}</time>
            </div>
          </div>
        </section>

        <section
          v-else
          id="release-panel-artifacts"
          class="release-tab-panel release-artifacts"
          role="tabpanel"
          aria-labelledby="release-tab-artifacts"
        >
          <div class="release-section-heading">
            <div>
              <h4>更新包</h4>
              <span>{{ selectedRelease.artifacts.length }} 个目标</span>
            </div>
            <button
              class="text-button"
              type="button"
              :disabled="Boolean(operation) || !canPublishArtifacts(selectedRelease)"
              @click="openUploadDrawer(selectedRelease)"
            ><Plus :size="15" />添加更新包</button>
          </div>

          <div v-if="selectedRelease.artifacts.length === 0" class="release-empty-row release-tab-empty">
            <FileArchive :size="18" />尚未添加更新包
          </div>
          <div v-else class="artifact-list">
            <article v-for="artifact in selectedRelease.artifacts" :key="artifact.id" class="artifact-row">
              <span class="artifact-icon"><FileArchive :size="17" /></span>
              <div class="artifact-name">
                <strong :title="artifact.file_name">{{ artifact.file_name }}</strong>
                <span>{{ artifact.os }} / {{ artifact.native_arch }} / {{ artifact.package_type }}</span>
              </div>
              <span :class="['artifact-status', artifact.status]">
                {{ artifact.status === 'published' ? '已发布' : '待发布' }}
              </span>
              <div class="artifact-integrity">
                <span>{{ formatBytes(artifact.size_bytes) }}</span>
                <code :title="artifact.sha256">{{ artifact.sha256.slice(0, 12) }}</code>
              </div>
              <div class="artifact-row-actions">
                <button
                  class="icon-button subtle"
                  type="button"
                  title="下载更新包"
                  :aria-label="`下载 ${artifact.file_name}`"
                  @click="downloadArtifact(selectedRelease.id, artifact)"
                ><Download :size="15" /></button>
                <button
                  v-if="artifact.status === 'draft'"
                  class="icon-button danger"
                  type="button"
                  title="移除更新包"
                  :aria-label="`移除 ${artifact.file_name}`"
                  :disabled="Boolean(operation)"
                  @click="$emit('deleteArtifact', selectedRelease.id, artifact.id)"
                >
                  <LoaderCircle v-if="isBusy(artifact.id) && operation === 'deleting'" class="spin" :size="15" />
                  <Trash2 v-else :size="15" />
                </button>
              </div>
            </article>
          </div>
        </section>
      </article>
    </section>
    <WorkspaceDrawer
      v-if="operationDrawer"
      :key="operationDrawer"
      :title="operationDrawerTitle"
      :description="operationDrawerDescription"
      :size="operationDrawer === 'upload' ? 'wide' : 'medium'"
      :modal="true"
      :busy="Boolean(operation)"
      @close="closeOperationDrawer"
    >
      <form v-if="operationDrawer === 'create'" id="create-agent-release-form" class="drawer-form" @submit.prevent="submitCreateRelease">
        <label class="release-create-file-picker">
          <span>更新文件 <i>可选，用于识别版本</i></span>
          <span
            class="file-button"
            :class="{ dragging: isFileDragActive(createReleaseDropKey), disabled: operation }"
            :title="createReleaseFiles.map((file) => file.name).join('\n')"
            @dragenter="fileDragEnter(createReleaseDropKey, $event)"
            @dragleave="fileDragLeave(createReleaseDropKey)"
            @dragover="fileDragOver"
            @drop.prevent="dropCreateReleaseFiles"
          >
            <Upload :size="18" />
            <span>
              <strong>{{ isFileDragActive(createReleaseDropKey) ? '松开即可添加文件' : createReleaseFileSummary }}</strong>
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
          <small v-if="createReleaseFileError" class="artifact-file-error" role="alert">{{ createReleaseFileError }}</small>
        </label>
        <label>
          <span>版本号</span>
          <input
            v-model.trim="form.version"
            required
            autofocus
            placeholder="例如：1.4.0"
            autocomplete="off"
            :disabled="createReleaseVersionConflict"
          />
          <small v-if="createReleaseVersionSource" class="release-version-hint">
            已从 {{ createReleaseVersionSource }} 识别，可直接修改
          </small>
        </label>
        <label>
          <span>发布说明 <i>可选</i></span>
          <textarea v-model.trim="form.notes" rows="6" placeholder="本次更新内容"></textarea>
        </label>
      </form>

      <form
        v-else-if="operationDrawer === 'edit' && selectedRelease"
        id="edit-agent-release-form"
        class="drawer-form"
        @submit.prevent="saveRelease(selectedRelease.id)"
      >
        <label>
          <span>版本号</span>
          <input v-model.trim="draftEdits[selectedRelease.id].version" required autofocus autocomplete="off" />
        </label>
        <label>
          <span>发布说明 <i>可选</i></span>
          <textarea v-model.trim="draftEdits[selectedRelease.id].notes" rows="8"></textarea>
        </label>
      </form>

      <form
        v-else-if="operationDrawer === 'upload' && selectedRelease"
        id="upload-agent-artifact-form"
        class="artifact-upload-form"
        @submit.prevent="submitArtifacts(selectedRelease)"
      >
        <label
          :class="['artifact-batch-picker', { dragging: batchDragActive[selectedRelease.id], disabled: operation }]"
          @dragenter="batchDragEnter(selectedRelease.id, $event)"
          @dragleave="batchDragLeave(selectedRelease.id)"
          @dragover="fileDragOver"
          @drop.prevent="dropBatchFiles(selectedRelease.id, $event)"
        >
          <Upload :size="18" />
          <span>
            <strong>{{ batchDragActive[selectedRelease.id] ? '松开即可添加多个文件' : '批量添加更新包' }}</strong>
            <small>同时选择更新包及其同名 .sha256 文件</small>
          </span>
          <input
            :key="batchInputKeys[selectedRelease.id]"
            type="file"
            accept=".bin,.exe,.sha256"
            multiple
            :disabled="Boolean(operation)"
            @change="chooseBatchFiles(selectedRelease.id, $event)"
          />
        </label>
        <small v-if="batchFileErrors[selectedRelease.id]" class="artifact-file-error" role="alert">
          {{ batchFileErrors[selectedRelease.id] }}
        </small>

        <div class="artifact-upload-rows">
          <div v-for="row in uploadRowsFor(selectedRelease.id)" :key="row.id" class="artifact-upload-row">
            <div class="artifact-upload-target">
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
                <span>原生架构</span>
                <select v-model="row.native_arch" required @change="changeArtifactArchitecture(row)">
                  <option v-if="!row.native_arch" value="" disabled>请选择架构</option>
                  <option v-for="architecture in nativeArchitectures(row.os)" :key="architecture" :value="architecture">
                    {{ architecture }}
                  </option>
                </select>
              </label>
              <button
                class="icon-button danger"
                type="button"
                title="移除上传目标"
                :aria-label="`移除上传目标 ${row.file?.name || row.os}`"
                :disabled="Boolean(operation)"
                @click="removeUploadRow(selectedRelease.id, row.id)"
              ><Trash2 :size="15" /></button>
            </div>

            <label
              :class="['artifact-file-picker', { dragging: isFileDragActive(uploadRowDropKey(row.id, 'executable')), selected: row.file, disabled: operation }]"
              @dragenter="fileDragEnter(uploadRowDropKey(row.id, 'executable'), $event)"
              @dragleave="fileDragLeave(uploadRowDropKey(row.id, 'executable'))"
              @dragover="fileDragOver"
              @drop.prevent="dropArtifactFile(row, $event)"
            >
              <span>可执行文件</span>
              <span class="file-button" :title="row.file?.name">
                <Upload :size="17" />
                <span>
                  <strong>{{ row.file?.name || '选择单个文件' }}</strong>
                  <small>{{ row.inference === 'needs_target' ? '请确认系统和架构' : row.inference === 'needs_architecture' ? '请确认架构' : `仅支持 ${artifactAccept(row)} 文件` }}</small>
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

            <label
              :class="['artifact-checksum-picker', { dragging: isFileDragActive(uploadRowDropKey(row.id, 'checksum')), selected: row.checksum_file, disabled: operation }]"
              @dragenter="fileDragEnter(uploadRowDropKey(row.id, 'checksum'), $event)"
              @dragleave="fileDragLeave(uploadRowDropKey(row.id, 'checksum'))"
              @dragover="fileDragOver"
              @drop.prevent="dropChecksumFile(row, $event)"
            >
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
            <small v-if="row.error" class="artifact-file-error" role="alert">{{ row.error }}</small>
          </div>
        </div>
      </form>

      <template #footer>
        <template v-if="operationDrawer === 'create'">
          <button class="text-button" type="button" :disabled="Boolean(operation)" @click="closeOperationDrawer">
            <X :size="15" />取消
          </button>
          <button
            class="primary-button"
            type="submit"
            form="create-agent-release-form"
            :disabled="Boolean(operation) || createReleaseVersionConflict || !form.version.trim()"
          >
            <LoaderCircle v-if="operation === 'creating'" class="spin" :size="16" />
            <Plus v-else :size="16" />{{ operation === 'creating' ? '正在创建' : '创建草稿' }}
          </button>
        </template>
        <template v-else-if="operationDrawer === 'edit' && selectedRelease">
          <button class="text-button" type="button" :disabled="Boolean(operation)" @click="cancelEdit(selectedRelease.id)">
            <X :size="15" />取消
          </button>
          <button class="primary-button" type="submit" form="edit-agent-release-form" :disabled="Boolean(operation)">
            <LoaderCircle v-if="isBusy(selectedRelease.id) && operation === 'saving'" class="spin" :size="15" />
            <Save v-else :size="15" />保存草稿
          </button>
        </template>
        <template v-else-if="operationDrawer === 'upload' && selectedRelease">
          <button class="text-button" type="button" :disabled="Boolean(operation)" @click="addUploadRow(selectedRelease.id)">
            <Plus :size="15" />添加目标
          </button>
          <button
            class="primary-button"
            type="submit"
            form="upload-agent-artifact-form"
            :disabled="Boolean(operation) || !uploadableRowsFor(selectedRelease.id).length"
          >
            <LoaderCircle v-if="isBusy(selectedRelease.id) && operation === 'uploading'" class="spin" :size="15" />
            <Upload v-else :size="15" />上传 {{ uploadableRowsFor(selectedRelease.id).length }} 个
          </button>
        </template>
      </template>
    </WorkspaceDrawer>
  </section>

  <AgentRolloutSelectorModal
    v-if="rolloutSelector"
    :release="rolloutSelector.release"
    :mode="rolloutSelector.mode"
    :candidates="rolloutCandidates[rolloutSelector.release.id] || []"
    :loading="rolloutCandidatesLoading === rolloutSelector.release.id"
    @close="rolloutSelector = null"
    @confirm="confirmRolloutTargets"
  />
</template>
