<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import {
  AlertTriangle,
  ArrowUp,
  CheckCircle2,
  ChevronRight,
  Download,
  File as FileIcon,
  FileQuestion,
  Folder,
  FolderOpen,
  FolderPlus,
  HardDrive,
  Link,
  LoaderCircle,
  LockKeyhole,
  Move,
  RefreshCw,
  Trash2,
  Upload,
  X,
} from 'lucide-vue-next'
import { ApiError, api } from '../api/http'
import {
  UploadCancelledError,
  UploadHttpError,
  instanceFileDownloadUrl,
  uploadInstanceFile,
} from '../api/files'
import type {
  FileOperationResult,
  FileRootsResponse,
  Instance,
  InstanceFileEntry,
  InstanceFileListing,
} from '../types/domain'
import { formatBytes, formatTime } from '../utils/format'

type UploadStatus = 'queued' | 'uploading' | 'completed' | 'cancelled' | 'error'

type UploadQueueItem = {
  id: string
  file: File
  status: UploadStatus
  progress: number
  transferred: number
  error: string
}

type FileDialog =
  | { type: 'create'; name: string; error: string }
  | {
      type: 'move'
      entry: InstanceFileEntry
      destinationParent: string
      name: string
      conflict: boolean
      error: string
    }
  | { type: 'delete'; entry: InstanceFileEntry; error: string }

const props = defineProps<{
  instance: Instance
}>()

const roots = ref<FileRootsResponse['roots']>([])
const maxFileBytes = ref(1024 * 1024 * 1024)
const listing = ref<InstanceFileListing | null>(null)
const currentPath = ref('')
const selectedRoot = ref('')
const loading = ref(false)
const operationBusy = ref(false)
const errorMessage = ref('')
const dragActive = ref(false)
const fileInput = ref<HTMLInputElement | null>(null)
const dialog = ref<FileDialog | null>(null)
const uploadQueue = ref<UploadQueueItem[]>([])
const processingUploads = ref(false)
const activeUploadId = ref('')
const activeUploadAbort = ref<(() => void) | null>(null)
const uploadConflict = ref<{
  fileName: string
  resolve: (overwrite: boolean) => void
} | null>(null)
let disposed = false

const dialogOpen = computed(() => Boolean(dialog.value || uploadConflict.value))

const hasMore = computed(() => {
  if (!listing.value) return false
  return listing.value.entries.length < listing.value.total
})

const breadcrumbs = computed(() => buildBreadcrumbs(currentPath.value))

watch(
  () => props.instance.id,
  () => void loadRoots(),
)

onMounted(() => void loadRoots())

onBeforeUnmount(() => {
  disposed = true
  for (const item of uploadQueue.value) {
    if (item.status === 'queued') item.status = 'cancelled'
  }
  activeUploadAbort.value?.()
  uploadConflict.value?.resolve(false)
})

async function loadRoots() {
  loading.value = true
  errorMessage.value = ''
  listing.value = null
  try {
    const response = await api<FileRootsResponse>(
      `/api/admin/instances/${encodeURIComponent(props.instance.id)}/files/roots`,
    )
    roots.value = response.roots
    maxFileBytes.value = response.max_file_bytes
    const firstRoot = response.roots[0]?.path || ''
    selectedRoot.value = firstRoot
    if (firstRoot) await openPath(firstRoot)
  } catch (error) {
    errorMessage.value = errorText(error)
  } finally {
    loading.value = false
  }
}

async function openPath(path: string, append = false) {
  if (!path) return
  loading.value = true
  errorMessage.value = ''
  try {
    const offset = append && listing.value?.path === path
      ? listing.value.entries.length
      : 0
    const params = new URLSearchParams({
      path,
      offset: String(offset),
      limit: '200',
    })
    const response = await api<InstanceFileListing>(
      `/api/admin/instances/${encodeURIComponent(props.instance.id)}/files?${params}`,
    )
    if (append && listing.value?.path === response.path) {
      listing.value = {
        ...response,
        offset: 0,
        entries: [...listing.value.entries, ...response.entries],
      }
    } else {
      listing.value = response
    }
    currentPath.value = response.path
    selectedRoot.value = matchingRoot(response.path)
  } catch (error) {
    errorMessage.value = errorText(error)
  } finally {
    loading.value = false
  }
}

function matchingRoot(path: string) {
  return roots.value
    .filter((root) => pathStartsWith(path, root.path))
    .sort((left, right) => right.path.length - left.path.length)[0]?.path
    || roots.value[0]?.path
    || ''
}

function changeRoot() {
  if (selectedRoot.value) void openPath(selectedRoot.value)
}

function refresh() {
  if (currentPath.value) void openPath(currentPath.value)
}

function openEntry(entry: InstanceFileEntry) {
  if (entry.kind === 'directory' || entry.kind === 'symlink') {
    void openPath(entry.path)
  }
}

function showCreateDialog() {
  dialog.value = { type: 'create', name: '', error: '' }
}

function showMoveDialog(entry: InstanceFileEntry) {
  dialog.value = {
    type: 'move',
    entry,
    destinationParent: currentPath.value,
    name: entry.name,
    conflict: false,
    error: '',
  }
}

function showDeleteDialog(entry: InstanceFileEntry) {
  dialog.value = { type: 'delete', entry, error: '' }
}

async function submitDialog(overwrite = false) {
  if (!dialog.value || operationBusy.value) return
  operationBusy.value = true
  dialog.value.error = ''
  try {
    if (dialog.value.type === 'create') {
      await api<FileOperationResult>(
        `/api/admin/instances/${encodeURIComponent(props.instance.id)}/directories`,
        {
          method: 'POST',
          body: JSON.stringify({ parent: currentPath.value, name: dialog.value.name.trim() }),
        },
      )
    } else if (dialog.value.type === 'move') {
      await api<FileOperationResult>(
        `/api/admin/instances/${encodeURIComponent(props.instance.id)}/files`,
        {
          method: 'PATCH',
          body: JSON.stringify({
            source: dialog.value.entry.path,
            destination_parent: dialog.value.destinationParent.trim(),
            name: dialog.value.name.trim(),
            overwrite,
          }),
        },
      )
    } else {
      await api<FileOperationResult>(
        `/api/admin/instances/${encodeURIComponent(props.instance.id)}/files`,
        {
          method: 'DELETE',
          body: JSON.stringify({
            path: dialog.value.entry.path,
            recursive: dialog.value.entry.kind === 'directory',
          }),
        },
      )
    }
    dialog.value = null
    await openPath(currentPath.value)
  } catch (error) {
    if (
      dialog.value?.type === 'move'
      && error instanceof ApiError
      && error.status === 409
      && /存在|同名/.test(error.message)
    ) {
      dialog.value.conflict = true
    }
    if (dialog.value) dialog.value.error = errorText(error)
  } finally {
    operationBusy.value = false
  }
}

function chooseFiles() {
  fileInput.value?.click()
}

function selectFiles(event: Event) {
  const input = event.target as HTMLInputElement
  enqueueFiles(Array.from(input.files || []))
  input.value = ''
}

function dropFiles(event: DragEvent) {
  dragActive.value = false
  const containsDirectory = Array.from(event.dataTransfer?.items || []).some((item) => {
    const entry = (item as DataTransferItem & {
      webkitGetAsEntry?: () => { isDirectory: boolean } | null
    }).webkitGetAsEntry?.()
    return entry?.isDirectory === true
  })
  if (containsDirectory) {
    errorMessage.value = '暂不支持递归上传文件夹，请选择其中的文件'
    return
  }
  enqueueFiles(Array.from(event.dataTransfer?.files || []))
}

function enqueueFiles(files: File[]) {
  errorMessage.value = ''
  for (const file of files) {
    const item: UploadQueueItem = {
      id: `${Date.now()}-${Math.random().toString(36).slice(2)}`,
      file,
      status: file.size <= maxFileBytes.value ? 'queued' : 'error',
      progress: 0,
      transferred: 0,
      error: file.size <= maxFileBytes.value
        ? ''
        : `文件超过 ${formatBytes(maxFileBytes.value)} 上限`,
    }
    uploadQueue.value.push(item)
  }
  void processUploadQueue()
}

async function processUploadQueue() {
  if (processingUploads.value) return
  processingUploads.value = true
  let uploaded = false
  try {
    let item = uploadQueue.value.find((candidate) => candidate.status === 'queued')
    while (item && !disposed) {
      activeUploadId.value = item.id
      item.status = 'uploading'
      try {
        await uploadQueueItem(item, false)
        item.status = 'completed'
        item.progress = 100
        uploaded = true
      } catch (error) {
        if (
          error instanceof UploadHttpError
          && error.status === 409
          && /存在|同名/.test(error.message)
        ) {
          const overwrite = await requestUploadOverwrite(item.file.name)
          if (overwrite) {
            try {
              await uploadQueueItem(item, true)
              item.status = 'completed'
              item.progress = 100
              uploaded = true
            } catch (retryError) {
              setUploadFailure(item, retryError)
            }
          } else {
            item.status = 'cancelled'
          }
        } else {
          setUploadFailure(item, error)
        }
      } finally {
        activeUploadAbort.value = null
        activeUploadId.value = ''
      }
      item = uploadQueue.value.find((candidate) => candidate.status === 'queued')
    }
  } finally {
    processingUploads.value = false
    if (uploaded && !disposed) await openPath(currentPath.value)
  }
}

async function uploadQueueItem(item: UploadQueueItem, overwrite: boolean) {
  if (disposed) throw new UploadCancelledError()
  item.status = 'uploading'
  item.error = ''
  const handle = uploadInstanceFile(
    props.instance.id,
    currentPath.value,
    item.file,
    overwrite,
    (transferred, total) => {
      item.transferred = transferred
      item.progress = total > 0 ? Math.min(100, (transferred / total) * 100) : 100
    },
  )
  activeUploadAbort.value = handle.abort
  await handle.promise
}

function setUploadFailure(item: UploadQueueItem, error: unknown) {
  if (error instanceof UploadCancelledError) {
    item.status = 'cancelled'
    return
  }
  item.status = 'error'
  item.error = errorText(error)
}

function cancelUpload(item: UploadQueueItem) {
  if (item.status === 'queued') {
    item.status = 'cancelled'
    return
  }
  if (item.status === 'uploading' && activeUploadId.value === item.id) {
    activeUploadAbort.value?.()
  }
}

function clearFinishedUploads() {
  uploadQueue.value = uploadQueue.value.filter((item) =>
    item.status === 'queued' || item.status === 'uploading',
  )
}

function requestUploadOverwrite(fileName: string) {
  return new Promise<boolean>((resolve) => {
    uploadConflict.value = { fileName, resolve }
  })
}

function resolveUploadOverwrite(overwrite: boolean) {
  const conflict = uploadConflict.value
  uploadConflict.value = null
  conflict?.resolve(overwrite)
}

function downloadEntry(entry: InstanceFileEntry) {
  const anchor = document.createElement('a')
  anchor.href = instanceFileDownloadUrl(props.instance.id, entry.path)
  anchor.download = entry.name
  document.body.appendChild(anchor)
  anchor.click()
  anchor.remove()
}

function entryIcon(entry: InstanceFileEntry) {
  if (entry.kind === 'directory') return Folder
  if (entry.kind === 'symlink') return Link
  if (entry.kind === 'file') return FileIcon
  return FileQuestion
}

function entryKindLabel(entry: InstanceFileEntry) {
  if (entry.kind === 'directory') return '目录'
  if (entry.kind === 'symlink') return '链接'
  if (entry.kind === 'file') return '文件'
  return '其他'
}

function errorText(error: unknown) {
  return error instanceof Error ? error.message : '文件操作失败'
}

function pathStartsWith(path: string, root: string) {
  return isWindowsPath(path)
    ? path.toLocaleLowerCase().startsWith(root.toLocaleLowerCase())
    : path.startsWith(root)
}

function isWindowsPath(path: string) {
  return /^[a-z]:[\\/]/i.test(path)
}

function buildBreadcrumbs(path: string) {
  if (!path) return []
  if (isWindowsPath(path)) {
    const normalized = path.replaceAll('/', '\\')
    const root = normalized.slice(0, 3)
    const segments = normalized.slice(3).split('\\').filter(Boolean)
    let current = root
    return [
      { label: root, path: root },
      ...segments.map((segment) => {
        current = `${current.replace(/\\$/, '')}\\${segment}`
        return { label: segment, path: current }
      }),
    ]
  }
  const segments = path.split('/').filter(Boolean)
  let current = ''
  return [
    { label: '/', path: '/' },
    ...segments.map((segment) => {
      current += `/${segment}`
      return { label: segment, path: current }
    }),
  ]
}
</script>

<template>
  <section
    :class="['file-manager', { dragging: dragActive, 'dialog-open': dialogOpen }]"
    @dragenter.prevent="dragActive = true"
    @dragover.prevent="dragActive = true"
    @dragleave.self="dragActive = false"
    @drop.prevent="dropFiles"
  >
    <div class="file-toolbar">
      <label class="file-root-select">
        <HardDrive :size="15" aria-hidden="true" />
        <select v-model="selectedRoot" aria-label="文件系统根目录" @change="changeRoot">
          <option v-for="root in roots" :key="root.path" :value="root.path">
            {{ root.label }}
          </option>
        </select>
      </label>

      <button
        class="icon-button subtle"
        type="button"
        title="返回上级目录"
        aria-label="返回上级目录"
        :disabled="!listing?.parent || loading"
        @click="listing?.parent && openPath(listing.parent)"
      >
        <ArrowUp :size="16" />
      </button>
      <button
        class="icon-button subtle"
        type="button"
        title="刷新目录"
        aria-label="刷新当前目录"
        :disabled="loading"
        @click="refresh"
      >
        <RefreshCw :class="{ spin: loading }" :size="16" />
      </button>

      <nav class="file-breadcrumbs" aria-label="当前文件路径">
        <template v-for="(crumb, index) in breadcrumbs" :key="crumb.path">
          <ChevronRight v-if="index" :size="13" aria-hidden="true" />
          <button type="button" :title="crumb.path" @click="openPath(crumb.path)">
            {{ crumb.label }}
          </button>
        </template>
      </nav>

      <div class="file-toolbar-actions">
        <button
          class="text-button"
          type="button"
          title="新建目录"
          :disabled="!currentPath || operationBusy"
          @click="showCreateDialog"
        >
          <FolderPlus :size="15" />新建目录
        </button>
        <button
          class="primary-button file-upload-trigger"
          type="button"
          title="上传文件"
          :disabled="!currentPath"
          @click="chooseFiles"
        >
          <Upload :size="15" />上传
        </button>
        <input ref="fileInput" type="file" multiple tabindex="-1" @change="selectFiles" />
      </div>
    </div>

    <p v-if="errorMessage" class="file-notice error" role="alert">
      <AlertTriangle :size="15" />{{ errorMessage }}
    </p>

    <div v-if="uploadQueue.length" class="upload-queue">
      <header>
        <strong>上传队列</strong>
        <button class="text-button" type="button" @click="clearFinishedUploads">
          清理已完成
        </button>
      </header>
      <div v-for="item in uploadQueue" :key="item.id" class="upload-item">
        <FileIcon :size="15" aria-hidden="true" />
        <div>
          <strong :title="item.file.name">{{ item.file.name }}</strong>
          <span>
            {{ formatBytes(item.file.size) }}
            <i v-if="item.error">{{ item.error }}</i>
          </span>
          <progress v-if="item.status === 'uploading'" :value="item.progress" max="100"></progress>
        </div>
        <span :class="['upload-status', item.status]">
          <LoaderCircle v-if="item.status === 'uploading'" class="spin" :size="14" />
          <CheckCircle2 v-else-if="item.status === 'completed'" :size="14" />
          {{ item.status === 'queued' ? '等待' : item.status === 'uploading' ? `${item.progress.toFixed(0)}%` : item.status === 'completed' ? '完成' : item.status === 'cancelled' ? '已取消' : '失败' }}
        </span>
        <button
          v-if="item.status === 'queued' || item.status === 'uploading'"
          class="icon-button subtle"
          type="button"
          title="取消上传"
          aria-label="取消上传"
          @click="cancelUpload(item)"
        >
          <X :size="14" />
        </button>
      </div>
    </div>

    <div class="file-list-shell">
      <div class="file-list-head" aria-hidden="true">
        <span>名称</span><span>大小</span><span>修改时间</span><span>操作</span>
      </div>
      <div v-if="loading && !listing" class="file-loading">
        <LoaderCircle class="spin" :size="22" />正在读取文件目录
      </div>
      <div v-else-if="listing && !listing.entries.length" class="file-empty">
        <FolderOpen :size="24" />
        <strong>目录为空</strong>
      </div>
      <div v-else-if="listing" class="file-list" role="list">
        <div
          v-for="entry in listing.entries"
          :key="entry.path"
          class="file-row"
          role="listitem"
          @dblclick="openEntry(entry)"
        >
          <button
            class="file-name"
            type="button"
            :title="entry.path"
            :disabled="entry.kind !== 'directory' && entry.kind !== 'symlink'"
            @click="openEntry(entry)"
          >
            <component :is="entryIcon(entry)" :size="17" aria-hidden="true" />
            <span class="file-entry-label">{{ entry.name }}</span>
            <span class="file-lock"><LockKeyhole v-if="entry.readonly" :size="12" aria-label="只读" /></span>
            <small>{{ entryKindLabel(entry) }}</small>
          </button>
          <span class="file-size">{{ entry.kind === 'file' ? formatBytes(entry.size_bytes) : '—' }}</span>
          <time>{{ formatTime(entry.modified_at) }}</time>
          <div class="file-row-actions">
            <button
              v-if="entry.kind !== 'directory'"
              class="icon-button subtle"
              type="button"
              title="下载"
              :aria-label="`下载 ${entry.name}`"
              @click="downloadEntry(entry)"
            >
              <Download :size="15" />
            </button>
            <button
              class="icon-button subtle"
              type="button"
              title="重命名或移动"
              :aria-label="`重命名或移动 ${entry.name}`"
              @click="showMoveDialog(entry)"
            >
              <Move :size="15" />
            </button>
            <button
              class="icon-button subtle danger"
              type="button"
              title="永久删除"
              :aria-label="`永久删除 ${entry.name}`"
              @click="showDeleteDialog(entry)"
            >
              <Trash2 :size="15" />
            </button>
          </div>
        </div>
      </div>
      <button
        v-if="hasMore"
        class="file-load-more"
        type="button"
        :disabled="loading"
        @click="openPath(currentPath, true)"
      >
        {{ loading ? '正在读取…' : `加载更多（${listing?.entries.length} / ${listing?.total}）` }}
      </button>
    </div>

    <div v-if="dragActive" class="file-drop-overlay" aria-hidden="true">
      <Upload :size="28" />
      <strong>释放以上传到当前目录</strong>
    </div>

    <div
      v-if="dialog"
      class="file-dialog-backdrop"
      @click.self="dialog = null"
      @wheel.self.prevent
      @touchmove.self.prevent
    >
      <form class="file-dialog" @submit.prevent="submitDialog(false)">
        <button class="icon-button subtle file-dialog-close" type="button" title="关闭" @click="dialog = null">
          <X :size="15" />
        </button>
        <template v-if="dialog.type === 'create'">
          <span class="file-dialog-icon"><FolderPlus :size="22" /></span>
          <h3>新建目录</h3>
          <label>
            <span>目录名称</span>
            <input v-model="dialog.name" required maxlength="255" autocomplete="off" />
          </label>
        </template>
        <template v-else-if="dialog.type === 'move'">
          <span class="file-dialog-icon"><Move :size="22" /></span>
          <h3>重命名或移动</h3>
          <label>
            <span>目标目录</span>
            <input v-model="dialog.destinationParent" required autocomplete="off" />
          </label>
          <label>
            <span>名称</span>
            <input v-model="dialog.name" required maxlength="255" autocomplete="off" />
          </label>
          <p v-if="dialog.conflict" class="file-dialog-warning">
            <AlertTriangle :size="14" />目标已存在。仅普通文件可以确认覆盖，目录不会被覆盖。
          </p>
        </template>
        <template v-else>
          <span class="file-dialog-icon danger"><Trash2 :size="22" /></span>
          <h3>永久删除</h3>
          <p class="file-delete-path">{{ dialog.entry.path }}</p>
          <p class="file-dialog-warning">
            <AlertTriangle :size="14" />{{ dialog.entry.kind === 'directory' ? '目录及其中全部内容将被递归删除，且无法恢复。' : '该文件将被永久删除，且无法恢复。' }}
          </p>
        </template>
        <p v-if="dialog.error" class="file-notice error">{{ dialog.error }}</p>
        <div class="file-dialog-actions">
          <button class="text-button" type="button" @click="dialog = null">取消</button>
          <button
            v-if="dialog.type === 'move' && dialog.conflict"
            class="confirm-button danger"
            type="button"
            :disabled="operationBusy"
            @click="submitDialog(true)"
          >
            确认覆盖
          </button>
          <button
            v-else
            :class="dialog.type === 'delete' ? 'confirm-button danger' : 'primary-button'"
            type="submit"
            :disabled="operationBusy"
          >
            {{ operationBusy ? '处理中…' : dialog.type === 'delete' ? '永久删除' : dialog.type === 'create' ? '创建目录' : '确认移动' }}
          </button>
        </div>
      </form>
    </div>

    <div
      v-if="uploadConflict"
      class="file-dialog-backdrop"
      @wheel.self.prevent
      @touchmove.self.prevent
    >
      <section class="file-dialog" role="alertdialog" aria-modal="true">
        <span class="file-dialog-icon danger"><AlertTriangle :size="22" /></span>
        <h3>目标文件已存在</h3>
        <p class="file-delete-path">{{ uploadConflict.fileName }}</p>
        <p class="file-dialog-warning">覆盖只会在新文件完整上传后执行。</p>
        <div class="file-dialog-actions">
          <button class="text-button" type="button" @click="resolveUploadOverwrite(false)">跳过</button>
          <button class="confirm-button danger" type="button" @click="resolveUploadOverwrite(true)">确认覆盖</button>
        </div>
      </section>
    </div>
  </section>
</template>
