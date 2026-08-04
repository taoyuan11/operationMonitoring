<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue'
import {
  ArrowUpCircle,
  Check,
  ChevronDown,
  CircleAlert,
  Clock3,
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
} from 'lucide-vue-next'
import AgentRolloutSelectorModal from './AgentRolloutSelectorModal.vue'
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
const collapsedReleases = reactive<Record<string, boolean>>({})
const editingReleaseId = ref<string | null>(null)
const rolloutSelector = ref<{ release: AgentRelease; mode: 'publish' | 'add' } | null>(null)
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
      if (!(release.id in collapsedReleases)) collapsedReleases[release.id] = true
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
  collapsedReleases[releaseId] = false
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
  artifactUploadRows[releaseId] = rows.length ? rows : [createUploadRow('linux', 'x86_64')]
}

function editRelease(release: AgentRelease) {
  draftEdits[release.id] = { version: release.version, notes: release.notes }
  collapsedReleases[release.id] = false
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

function isReleaseCollapsed(releaseId: string) {
  return collapsedReleases[releaseId] ?? true
}

function toggleRelease(releaseId: string) {
  collapsedReleases[releaseId] = !isReleaseCollapsed(releaseId)
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
          <span
            class="file-button"
            :class="{ dragging: isFileDragActive(createReleaseDropKey), disabled: operation }"
            :title="createReleaseFiles.map((file) => file.name).join('\n')"
            @dragenter="fileDragEnter(createReleaseDropKey, $event)"
            @dragleave="fileDragLeave(createReleaseDropKey)"
            @dragover="fileDragOver"
            @drop.prevent="dropCreateReleaseFiles"
          >
            <Upload :size="17" />
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
          <small v-if="createReleaseFileError" class="artifact-file-error" role="alert">
            {{ createReleaseFileError }}
          </small>
        </label>
        <label>
          <span>版本号</span>
          <input
            v-model.trim="form.version"
            required
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
          <textarea v-model.trim="form.notes" placeholder="本次更新内容"></textarea>
        </label>
        <button
          class="primary-button"
          type="submit"
          :disabled="Boolean(operation) || createReleaseVersionConflict || !form.version.trim()"
        >
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

      <article
        v-for="release in releases"
        :key="release.id"
        class="update-release-card"
        :class="{ collapsed: isReleaseCollapsed(release.id) }"
      >
        <header class="release-card-header">
          <div class="release-identity">
            <div class="release-badges">
              <span :class="['release-status', release.status]">{{ releaseStatusText[release.status] }}</span>
              <span
                v-if="release.status === 'published'"
                :class="['rollout-status', release.rollout_state]"
              >{{ rolloutStatusText[release.rollout_state] }}</span>
            </div>
            <div v-if="editingReleaseId !== release.id">
              <h3>Agent {{ release.version }}</h3>
              <p v-if="release.notes" class="release-notes" :title="release.notes">{{ release.notes }}</p>
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
            </template>
            <span class="release-delete-control" :title="deleteReleaseTitle(release)">
              <button
                class="icon-button danger"
                type="button"
                :aria-label="deleteReleaseLabel(release)"
                :disabled="Boolean(operation) || activeAttemptCount(release) > 0"
                @click="$emit('deleteRelease', release)"
              >
                <LoaderCircle v-if="isBusy(release.id) && operation === 'deleting'" class="spin" :size="15" />
                <Trash2 v-else :size="15" />
              </button>
            </span>
            <button
              v-if="draftArtifactCount(release) > 0"
              class="primary-button release-publish-button"
              type="button"
              :title="release.status === 'published' ? '发布新增更新包' : '发布更新'"
              :disabled="Boolean(operation) || !canPublishArtifacts(release)"
              @click="release.status === 'draft' ? openRolloutSelector(release, 'publish') : $emit('publishRelease', release)"
            >
              <LoaderCircle v-if="isBusy(release.id) && operation === 'publishing'" class="spin" :size="15" />
              <Send v-else :size="15" />{{ release.status === 'published' ? '发布新增包' : '发布' }}
            </button>
            <button
              class="icon-button release-collapse-button"
              :class="{ expanded: !isReleaseCollapsed(release.id) }"
              type="button"
              :title="isReleaseCollapsed(release.id) ? '展开版本详情' : '折叠版本详情'"
              :aria-label="`${isReleaseCollapsed(release.id) ? '展开' : '折叠'} Agent ${release.version} 版本详情`"
              :aria-expanded="!isReleaseCollapsed(release.id)"
              :aria-controls="`release-body-${release.id}`"
              @click="toggleRelease(release.id)"
            >
              <ChevronDown :size="16" />
            </button>
          </div>
        </header>

        <div
          v-show="!isReleaseCollapsed(release.id)"
          :id="`release-body-${release.id}`"
          class="release-card-body"
        >
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

          <section v-if="release.status === 'published'" class="release-rollout-control">
            <div class="release-rollout-summary">
              <span :class="['rollout-state-icon', release.rollout_state]">
                <Pause v-if="isRolloutPaused(release)" :size="16" />
                <Undo2 v-else-if="['rollback_active', 'rolled_back', 'rollback_partial'].includes(release.rollout_state)" :size="16" />
                <Send v-else :size="16" />
              </span>
              <div>
                <strong>{{ rolloutStatusText[release.rollout_state] }}</strong>
                <small>
                  {{ isCanaryRollout(release) ? `${release.coverage.selected_instances} 个灰度目标` : '全量发布策略' }}
                  <template v-if="release.rollout_updated_at"> · {{ formatTime(release.rollout_updated_at) }}</template>
                </small>
              </div>
            </div>
            <div class="release-rollout-actions">
              <button
                v-if="isCanaryRollout(release)"
                class="text-button"
                type="button"
                title="添加灰度批次"
                :disabled="Boolean(operation)"
                @click="openRolloutSelector(release, 'add')"
              >
                <UserPlus :size="15" />添加批次
              </button>
              <button
                v-if="isRolloutActive(release)"
                class="text-button"
                type="button"
                title="暂停尚未下发的任务"
                :disabled="Boolean(operation)"
                @click="$emit('pauseRollout', release)"
              >
                <Pause :size="15" />暂停
              </button>
              <button
                v-if="isRolloutPaused(release)"
                class="text-button"
                type="button"
                title="恢复发布"
                :disabled="Boolean(operation)"
                @click="$emit('resumeRollout', release)"
              >
                <Play :size="15" />恢复
              </button>
              <button
                v-if="isCanaryRollout(release)"
                class="text-button"
                type="button"
                title="晋级为全量发布"
                :disabled="Boolean(operation)"
                @click="$emit('promoteRollout', release)"
              >
                <ArrowUpCircle :size="15" />晋级全量
              </button>
              <button
                v-if="canRollbackRelease(release)"
                class="text-button danger"
                type="button"
                title="批量回滚此版本"
                :disabled="Boolean(operation)"
                @click="$emit('rollbackRelease', release)"
              >
                <Undo2 :size="15" />批量回滚
              </button>
            </div>
          </section>

          <div class="release-coverage" :aria-label="`Agent ${release.version} 覆盖情况`">
            <span><strong>{{ release.coverage.selected_instances }}</strong> 显式目标</span>
            <span><strong>{{ release.coverage.covered_instances }}</strong> / {{ release.coverage.eligible_instances }} 平台覆盖</span>
            <span><strong>{{ attemptStats(release).upgradeSucceeded }}</strong> 升级成功</span>
            <span><strong>{{ attemptStats(release).rollbackSucceeded }}</strong> 回滚成功</span>
            <span><strong>{{ attemptStats(release).active }}</strong> 处理中</span>
            <span :class="{ warning: attemptStats(release).failed > 0 }">
              <strong>{{ attemptStats(release).failed }}</strong> 失败
            </span>
            <span v-if="release.rollback_coverage.succeeded_upgrades > 0">
              <strong>{{ release.rollback_coverage.rollback_supported }}</strong> / {{ release.rollback_coverage.succeeded_upgrades }} 支持回滚协议
            </span>
            <span
              v-if="release.rollback_coverage.unavailable > 0"
              class="warning"
            >
              <ShieldAlert :size="14" />{{ release.rollback_coverage.unavailable }} 不可回滚
            </span>
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
              <span
                :class="['artifact-status', artifact.status]"
                :title="artifact.status === 'published' ? `发布于 ${formatTime(artifact.published_at)}` : '再次确认发布后才会向实例推送'"
              >
                {{ artifact.status === 'published' ? '已发布' : '待发布' }}
              </span>
              <div class="artifact-integrity">
                <span>{{ formatBytes(artifact.size_bytes) }}</span>
                <code :title="artifact.sha256">{{ artifact.sha256.slice(0, 12) }}</code>
              </div>
              <button
                v-if="artifact.status === 'draft'"
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

          <form class="artifact-upload-form" @submit.prevent="submitArtifacts(release)">
            <div class="artifact-upload-toolbar">
              <label
                :class="[
                  'artifact-batch-picker',
                  { dragging: batchDragActive[release.id], disabled: operation },
                ]"
                @dragenter="batchDragEnter(release.id, $event)"
                @dragleave="batchDragLeave(release.id)"
                @dragover="fileDragOver"
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
                <label
                  :class="[
                    'artifact-file-picker',
                    {
                      dragging: isFileDragActive(uploadRowDropKey(row.id, 'executable')),
                      selected: row.file,
                      disabled: operation,
                    },
                  ]"
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
                <label
                  :class="[
                    'artifact-checksum-picker',
                    {
                      dragging: isFileDragActive(uploadRowDropKey(row.id, 'checksum')),
                      selected: row.checksum_file,
                      disabled: operation,
                    },
                  ]"
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
                <span>实例</span><span>操作</span><span>版本</span><span>状态</span><span>说明</span><span>更新时间</span><span></span>
              </div>
              <article v-for="attempt in attemptsFor(release)" :key="attempt.id" class="update-attempt-row">
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
                <span :class="['attempt-status', attempt.status]" :title="attemptStatusText[attempt.status]">
                  {{ attemptStatusText[attempt.status] }}
                </span>
                <span class="attempt-message" :title="attempt.message || '暂无补充说明'">{{ attempt.message || '—' }}</span>
                <time>{{ formatTime(attempt.updated_at) }}</time>
                <div class="attempt-actions">
                  <button
                    v-if="canRetryAttempt(release, attempt)"
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
                    v-else-if="canRollbackInstance(release, attempt)"
                    class="icon-button danger"
                    type="button"
                    title="回滚此实例"
                    :aria-label="`将实例 ${attempt.instance_id} 回滚到 ${attempt.from_version}`"
                    :disabled="Boolean(operation)"
                    @click="$emit('rollbackInstance', release, attempt)"
                  >
                    <LoaderCircle v-if="isBusy(attempt.id) && operation === 'rolling_back'" class="spin" :size="15" />
                    <Undo2 v-else :size="15" />
                  </button>
                  <button
                    v-else-if="canReupgradeInstance(release, attempt)"
                    class="icon-button"
                    type="button"
                    title="重新升级此实例"
                    :aria-label="`将实例 ${attempt.instance_id} 重新升级到 ${release.version}`"
                    :disabled="Boolean(operation)"
                    @click="$emit('reupgradeInstance', release, attempt)"
                  >
                    <LoaderCircle v-if="isBusy(attempt.id) && operation === 'reupgrading'" class="spin" :size="15" />
                    <RefreshCw v-else :size="15" />
                  </button>
                </div>
              </article>
            </div>
          </section>
        </div>
      </article>
    </section>
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
