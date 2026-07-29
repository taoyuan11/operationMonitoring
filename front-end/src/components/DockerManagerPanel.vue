<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue'
import {
  AlertTriangle,
  Archive,
  Box,
  Boxes,
  CircleGauge,
  Database,
  Eye,
  HardDrive,
  Images,
  Link2,
  LoaderCircle,
  Network,
  PackageOpen,
  Pause,
  Pencil,
  Play,
  Plus,
  RefreshCw,
  RotateCw,
  ScrollText,
  Search,
  Server,
  ShieldAlert,
  Skull,
  Square,
  Tag,
  Terminal,
  Trash2,
  Unlink,
  X,
} from 'lucide-vue-next'
import {
  connectDockerNetwork,
  createDockerContainer,
  createDockerNetwork,
  createDockerVolume,
  deleteDockerContainer,
  deleteDockerImage,
  deleteDockerNetwork,
  deleteDockerVolume,
  deployDockerCompose,
  disconnectDockerNetwork,
  getDockerContainer,
  getDockerContainerStats,
  getDockerDiskUsage,
  getDockerImage,
  getDockerNetwork,
  getDockerStatus,
  getDockerVolume,
  getDockerComposeProject,
  listDockerComposeProjects,
  listDockerContainers,
  listDockerImages,
  listDockerNetworks,
  listDockerVolumes,
  pruneDockerResource,
  pruneDockerSystem,
  pullDockerImage,
  renameDockerContainer,
  runDockerComposeAction,
  runDockerContainerAction,
  tagDockerImage,
  validateDockerCompose,
} from '../api/docker'
import type { Instance } from '../types/domain'
import type {
  DockerComposeProject,
  DockerComposeRequest,
  DockerComposeValidation,
  DockerContainer,
  DockerContainerCreateInput,
  DockerContainerStats,
  DockerDiskUsage,
  DockerImage,
  DockerNetwork,
  DockerOperationResult,
  DockerStatus,
  DockerVolume,
} from '../types/docker'
import { formatBytes, formatTime } from '../utils/format'
import DockerComposeValidationPreview from './DockerComposeValidationPreview.vue'
import DockerExecTerminal from './DockerExecTerminal.vue'
import DockerLogsPanel from './DockerLogsPanel.vue'

type DockerView = 'containers' | 'images' | 'networks' | 'volumes' | 'compose' | 'system'
type DialogKind =
  | 'create-container'
  | 'pull-image'
  | 'tag-image'
  | 'rename-container'
  | 'create-network'
  | 'network-membership'
  | 'create-volume'
  | 'compose-deploy'
  | 'compose-action'
  | 'exec-shell'
  | 'confirm'

const props = defineProps<{
  instance: Instance
  status: DockerStatus
}>()

const emit = defineEmits<{
  status: [status: DockerStatus]
}>()

const view = ref<DockerView>('containers')
const localStatus = ref<DockerStatus>(props.status)
const loadingView = ref<DockerView | null>(null)
const loading = computed(() => loadingView.value === view.value)
const operationBusy = ref(false)
const errorMessage = ref('')
const successMessage = ref('')
const search = ref('')
const containerState = ref('all')
const containers = ref<DockerContainer[]>([])
const containerStats = ref<Record<string, DockerContainerStats>>({})
const images = ref<DockerImage[]>([])
const networks = ref<DockerNetwork[]>([])
const volumes = ref<DockerVolume[]>([])
const projects = ref<DockerComposeProject[]>([])
const diskUsage = ref<DockerDiskUsage | null>(null)
const dialog = ref<DialogKind | null>(null)
const detailTitle = ref('')
const detailData = ref<unknown>(null)
const detailLoading = ref(false)
const logContainer = ref<DockerContainer | null>(null)
const terminalContainer = ref<DockerContainer | null>(null)
const selectedShell = ref<'/bin/sh' | '/bin/bash' | '/bin/ash'>('/bin/sh')
let pollTimer: number | null = null
let disposed = false
let pollInFlight = false
const resourceRequests: Record<DockerView, number> = {
  containers: 0,
  images: 0,
  networks: 0,
  volumes: 0,
  compose: 0,
  system: 0,
}
let detailRequest = 0
let composeValidationRequest = 0
let composeActionValidationRequest = 0

const createContainerForm = reactive({
  name: '',
  image: '',
  command: '',
  environment: '',
  ports: '',
  volumes: '',
  bindMounts: '',
  network: '',
  restartPolicy: 'unless-stopped' as DockerContainerCreateInput['restart_policy'],
  cpus: 0,
  memoryMb: 0,
  confirmReadWriteBindMounts: false,
})
const pullImageReference = ref('')
const tagImageTarget = ref<DockerImage | null>(null)
const tagImageForm = reactive({ repository: '', tag: 'latest' })
const renameContainerTarget = ref<DockerContainer | null>(null)
const renameContainerName = ref('')
const createNetworkForm = reactive({ name: '', driver: 'bridge', internal: false })
const networkMembership = reactive({
  network: null as DockerNetwork | null,
  mode: 'connect' as 'connect' | 'disconnect',
  container: '',
  aliases: '',
  force: false,
})
const createVolumeForm = reactive({ name: '', driver: 'local' })
const composeForm = reactive({
  projectName: '',
  files: '',
  profiles: '',
  services: '',
  confirmRisks: false,
})
const composeValidation = ref<DockerComposeValidation | null>(null)
const composeActionValidation = ref<DockerComposeValidation | null>(null)
const composeActionValidationLoading = ref(false)
const composeAction = reactive({
  project: null as DockerComposeProject | null,
  action: 'up' as 'pull' | 'up' | 'start' | 'stop' | 'restart' | 'down',
  profiles: '',
  services: '',
  removeVolumes: false,
  confirmRisks: false,
})
const confirmDialog = reactive({
  title: '',
  message: '',
  label: '',
  target: '',
  typed: '',
  danger: true,
  options: 'generic' as 'generic' | 'container-delete',
  action: null as null | (() => Promise<unknown>),
})
const deleteContainerOptions = reactive({ force: false, removeVolumes: false })

const manageable = computed(() => props.instance.online && localStatus.value.manageable)
const composeAvailable = computed(() => Boolean(localStatus.value.compose_version))
const normalizedSearch = computed(() => search.value.trim().toLowerCase())
const filteredContainers = computed(() => containers.value.filter((container) => {
  if (containerState.value !== 'all' && normalizedState(container) !== containerState.value) return false
  const text = `${containerName(container)} ${container.id} ${container.image} ${container.status}`.toLowerCase()
  return text.includes(normalizedSearch.value)
}))
const filteredImages = computed(() => images.value.filter((image) =>
  `${image.id} ${(image.repo_tags || []).join(' ')} ${(image.repo_digests || []).join(' ')}`
    .toLowerCase()
    .includes(normalizedSearch.value),
))
const filteredNetworks = computed(() => networks.value.filter((network) =>
  `${network.name} ${network.id} ${network.driver} ${network.scope || ''}`
    .toLowerCase()
    .includes(normalizedSearch.value),
))
const filteredVolumes = computed(() => volumes.value.filter((volume) =>
  `${volume.name} ${volume.driver} ${volume.mountpoint || ''}`
    .toLowerCase()
    .includes(normalizedSearch.value),
))
const filteredProjects = computed(() => projects.value.filter((project) =>
  `${project.name} ${project.status || ''}`.toLowerCase().includes(normalizedSearch.value),
))
const dockerDiagnostic = computed(() => {
  if (!props.instance.online) return '实例当前离线，资源清单已清空。Agent 重连后可继续管理。'
  if (!localStatus.value.protocol_supported) return '当前 Agent 不支持 Docker 管理协议，请先更新 Agent。'
  return localStatus.value.diagnostic || statusLabel(localStatus.value.status)
})

watch(() => props.status, (status) => {
  localStatus.value = status
  if (!status.compose_version && view.value === 'compose') view.value = 'containers'
  if (!manageable.value) resetUnavailableState()
})
watch(view, (_, previousView) => {
  invalidateResourceRequest(previousView)
  search.value = ''
  closeStreams()
  closeDetails()
  void refreshCurrent()
})
watch(
  () => createContainerForm.bindMounts,
  () => { createContainerForm.confirmReadWriteBindMounts = false },
)
watch(
  () => props.instance.online,
  (online) => {
    if (!online) {
      resetUnavailableState()
      return
    }
    void refreshStatusAndResources()
  },
)
watch(
  [
    () => composeForm.projectName,
    () => composeForm.files,
    () => composeForm.profiles,
    () => composeForm.services,
  ],
  () => {
    composeValidationRequest += 1
    composeValidation.value = null
    composeForm.confirmRisks = false
  },
)
watch(
  [
    () => composeAction.project?.name,
    () => composeAction.action,
    () => composeAction.profiles,
    () => composeAction.services,
  ],
  () => {
    composeActionValidationRequest += 1
    composeActionValidationLoading.value = false
    composeActionValidation.value = null
    composeAction.confirmRisks = false
  },
)

onMounted(() => {
  void refreshStatusAndResources()
  pollTimer = window.setInterval(() => {
    void pollContainers()
  }, 5000)
})

onBeforeUnmount(() => {
  disposed = true
  if (pollTimer != null) window.clearInterval(pollTimer)
  closeStreams()
})

async function refreshStatusAndResources() {
  await refreshStatus()
  if (manageable.value) await refreshCurrent()
}

async function pollContainers() {
  if (view.value !== 'containers' || pollInFlight || loading.value || operationBusy.value) return
  pollInFlight = true
  try {
    await refreshStatus()
    if (
      view.value === 'containers'
      && manageable.value
      && !loading.value
      && !operationBusy.value
    ) {
      await loadContainers(true)
    }
  } finally {
    pollInFlight = false
  }
}

async function refreshStatus() {
  try {
    const status = await getDockerStatus(props.instance.id)
    if (disposed) return
    localStatus.value = status
    emit('status', status)
    if (!status.manageable) resetUnavailableState()
  } catch (error) {
    if (!disposed) errorMessage.value = errorText(error)
  }
}

async function refreshCurrent() {
  if (!manageable.value) return
  const currentView = view.value
  if (currentView === 'containers') await loadContainers()
  if (currentView === 'images') await loadImages()
  if (currentView === 'networks') await loadNetworks()
  if (currentView === 'volumes') await loadVolumes()
  if (currentView === 'compose' && composeAvailable.value) await loadProjects()
  if (currentView === 'system') await loadDiskUsage()
}

async function loadContainers(silent = false) {
  const resource: DockerView = 'containers'
  const request = ++resourceRequests[resource]
  if (!silent) beginLoading(resource)
  try {
    const next = await listDockerContainers(props.instance.id)
    if (!isCurrentResourceRequest(resource, request)) return
    containers.value = next
    const running = next.filter((container) => normalizedState(container) === 'running')
    const entries = await mapLimit(running, 2, async (container) => {
      if (!isCurrentResourceRequest(resource, request)) return null
      try {
        return [container.id, await getDockerContainerStats(props.instance.id, container.id)] as const
      } catch {
        return null
      }
    })
    if (isCurrentResourceRequest(resource, request)) {
      containerStats.value = Object.fromEntries(entries.filter((entry) => entry !== null))
    }
  } catch (error) {
    if (!silent && isCurrentResourceRequest(resource, request)) {
      errorMessage.value = errorText(error)
    }
  } finally {
    if (!silent) finishLoading(resource, request)
  }
}

async function loadImages() {
  const resource: DockerView = 'images'
  const request = ++resourceRequests[resource]
  beginLoading(resource)
  try {
    const next = await listDockerImages(props.instance.id)
    if (isCurrentResourceRequest(resource, request)) images.value = next
  } catch (error) {
    if (isCurrentResourceRequest(resource, request)) errorMessage.value = errorText(error)
  } finally {
    finishLoading(resource, request)
  }
}

async function loadNetworks() {
  const resource: DockerView = 'networks'
  const request = ++resourceRequests[resource]
  beginLoading(resource)
  try {
    const next = await listDockerNetworks(props.instance.id)
    if (isCurrentResourceRequest(resource, request)) networks.value = next
  } catch (error) {
    if (isCurrentResourceRequest(resource, request)) errorMessage.value = errorText(error)
  } finally {
    finishLoading(resource, request)
  }
}

async function loadVolumes() {
  const resource: DockerView = 'volumes'
  const request = ++resourceRequests[resource]
  beginLoading(resource)
  try {
    const next = await listDockerVolumes(props.instance.id)
    if (isCurrentResourceRequest(resource, request)) volumes.value = next
  } catch (error) {
    if (isCurrentResourceRequest(resource, request)) errorMessage.value = errorText(error)
  } finally {
    finishLoading(resource, request)
  }
}

async function loadProjects() {
  const resource: DockerView = 'compose'
  const request = ++resourceRequests[resource]
  beginLoading(resource)
  try {
    const next = await listDockerComposeProjects(props.instance.id)
    if (isCurrentResourceRequest(resource, request)) projects.value = next
  } catch (error) {
    if (isCurrentResourceRequest(resource, request)) errorMessage.value = errorText(error)
  } finally {
    finishLoading(resource, request)
  }
}

async function loadDiskUsage() {
  const resource: DockerView = 'system'
  const request = ++resourceRequests[resource]
  beginLoading(resource)
  try {
    const next = await getDockerDiskUsage(props.instance.id)
    if (isCurrentResourceRequest(resource, request)) diskUsage.value = next
  } catch (error) {
    if (isCurrentResourceRequest(resource, request)) errorMessage.value = errorText(error)
  } finally {
    finishLoading(resource, request)
  }
}

function beginLoading(resource: DockerView) {
  loadingView.value = resource
  errorMessage.value = ''
}

function finishLoading(resource: DockerView, request: number) {
  if (resourceRequests[resource] === request && loadingView.value === resource) {
    loadingView.value = null
  }
}

function isCurrentResourceRequest(resource: DockerView, request: number) {
  return !disposed && manageable.value && resourceRequests[resource] === request
}

function invalidateResourceRequest(resource: DockerView) {
  resourceRequests[resource] += 1
  if (loadingView.value === resource) loadingView.value = null
}

function clearResources() {
  for (const resource of Object.keys(resourceRequests) as DockerView[]) {
    invalidateResourceRequest(resource)
  }
  containers.value = []
  containerStats.value = {}
  images.value = []
  networks.value = []
  volumes.value = []
  projects.value = []
  diskUsage.value = null
}

function closeDetails() {
  detailRequest += 1
  detailTitle.value = ''
  detailData.value = null
  detailLoading.value = false
}

function resetUnavailableState() {
  clearResources()
  closeStreams()
  closeDetails()
  dialog.value = null
  loadingView.value = null
}

function closeStreams() {
  logContainer.value = null
  terminalContainer.value = null
}

function openCreateForCurrentView() {
  if (view.value === 'containers') dialog.value = 'create-container'
  if (view.value === 'images') dialog.value = 'pull-image'
  if (view.value === 'networks') dialog.value = 'create-network'
  if (view.value === 'volumes') dialog.value = 'create-volume'
  if (view.value === 'compose') dialog.value = 'compose-deploy'
}

async function submitContainer() {
  try {
    const bindMounts = parseMounts(createContainerForm.bindMounts, true)
    const input: DockerContainerCreateInput = {
      name: createContainerForm.name.trim(),
      image: createContainerForm.image.trim(),
      command: splitLines(createContainerForm.command),
      environment: splitLines(createContainerForm.environment),
      ports: parsePorts(createContainerForm.ports),
      volumes: parseMounts(createContainerForm.volumes, false).map((mount) => ({
        name: mount.source,
        target: mount.target,
        readonly: mount.readonly,
      })),
      bind_mounts: bindMounts,
      network: createContainerForm.network.trim() || null,
      restart_policy: createContainerForm.restartPolicy,
      cpus: createContainerForm.cpus > 0 ? createContainerForm.cpus : null,
      memory_bytes: createContainerForm.memoryMb > 0
        ? Math.round(createContainerForm.memoryMb * 1024 * 1024)
        : null,
      confirm_read_write_bind_mounts: createContainerForm.confirmReadWriteBindMounts,
    }
    if (!input.image) throw new Error('镜像不能为空')
    if (bindMounts.some((mount) => !mount.readonly) && !input.confirm_read_write_bind_mounts) {
      throw new Error('读写 bind mount 需要显式确认')
    }
    const succeeded = await perform('容器已创建', () => createDockerContainer(props.instance.id, input))
    if (succeeded) {
      dialog.value = null
      resetContainerForm()
    }
  } catch (error) {
    errorMessage.value = errorText(error)
  }
}

async function containerAction(
  container: DockerContainer,
  action: 'start' | 'stop' | 'restart' | 'kill' | 'pause' | 'unpause',
) {
  const execute = () => perform(`${containerName(container)} 操作已完成`, () =>
    runDockerContainerAction(props.instance.id, container.id, action),
  )
  if (action === 'start' || action === 'restart' || action === 'pause' || action === 'unpause') {
    await execute()
    return
  }
  askConfirmation({
    title: action === 'kill' ? '终止容器' : '停止容器',
    message: action === 'kill'
      ? `将立即向 ${containerName(container)} 发送终止信号。`
      : `将请求 ${containerName(container)} 正常停止。`,
    label: action === 'kill' ? '立即终止' : '停止容器',
    target: containerName(container),
    action: execute,
  })
}

function confirmDeleteContainer(container: DockerContainer) {
  Object.assign(deleteContainerOptions, {
    force: normalizedState(container) === 'running',
    removeVolumes: false,
  })
  askConfirmation({
    title: '删除容器',
    message: `将永久删除 ${containerName(container)}。运行中的容器会被强制停止。`,
    label: '永久删除',
    target: containerName(container),
    options: 'container-delete',
    action: () => perform('容器已删除', () => deleteDockerContainer(props.instance.id, container.id, {
      force: deleteContainerOptions.force,
      remove_volumes: deleteContainerOptions.removeVolumes,
    })),
  })
}

function openRenameContainer(container: DockerContainer) {
  renameContainerTarget.value = container
  renameContainerName.value = containerName(container)
  dialog.value = 'rename-container'
}

async function submitRenameContainer() {
  const target = renameContainerTarget.value
  const name = renameContainerName.value.trim()
  if (!target || !name) return
  await perform('容器已重命名', () => renameDockerContainer(props.instance.id, target.id, name))
  if (!errorMessage.value) {
    dialog.value = null
    renameContainerTarget.value = null
  }
}

async function submitPullImage() {
  if (!pullImageReference.value.trim()) return
  await perform('镜像拉取完成', () => pullDockerImage(props.instance.id, pullImageReference.value.trim()))
  if (!errorMessage.value) {
    dialog.value = null
    pullImageReference.value = ''
  }
}

function openTagImage(image: DockerImage) {
  tagImageTarget.value = image
  const firstTag = image.repo_tags?.find((tag) => tag !== '<none>:<none>') || ''
  const separator = firstTag.lastIndexOf(':')
  tagImageForm.repository = separator > -1 ? firstTag.slice(0, separator) : firstTag
  tagImageForm.tag = separator > -1 ? firstTag.slice(separator + 1) : 'latest'
  dialog.value = 'tag-image'
}

async function submitTagImage() {
  if (!tagImageTarget.value || !tagImageForm.repository.trim() || !tagImageForm.tag.trim()) return
  await perform('镜像标签已创建', () => tagDockerImage(
    props.instance.id,
    tagImageTarget.value!.id,
    tagImageForm.repository.trim(),
    tagImageForm.tag.trim(),
  ))
  if (!errorMessage.value) dialog.value = null
}

function confirmDeleteImage(image: DockerImage) {
  const name = imageLabel(image)
  askConfirmation({
    title: '删除镜像',
    message: `将删除镜像 ${name}。被容器引用时操作可能失败。`,
    label: '删除镜像',
    target: name,
    action: () => perform('镜像已删除', () => deleteDockerImage(props.instance.id, image.id)),
  })
}

async function submitNetwork() {
  if (!createNetworkForm.name.trim()) return
  await perform('网络已创建', () => createDockerNetwork(
    props.instance.id,
    createNetworkForm.name.trim(),
    createNetworkForm.driver,
    createNetworkForm.internal,
  ))
  if (!errorMessage.value) {
    dialog.value = null
    Object.assign(createNetworkForm, { name: '', driver: 'bridge', internal: false })
  }
}

function openNetworkMembership(network: DockerNetwork, mode: 'connect' | 'disconnect') {
  Object.assign(networkMembership, { network, mode, container: '', aliases: '', force: false })
  dialog.value = 'network-membership'
}

async function submitNetworkMembership() {
  const network = networkMembership.network
  const container = networkMembership.container.trim()
  if (!network || !container) return
  if (networkMembership.mode === 'connect') {
    await perform('容器已连接到网络', () => connectDockerNetwork(
      props.instance.id,
      network.id,
      container,
      splitLines(networkMembership.aliases),
    ))
    if (!errorMessage.value) dialog.value = null
    return
  }
  const force = networkMembership.force
  const execute = () => perform('容器已断开网络', () => disconnectDockerNetwork(
      props.instance.id,
      network.id,
      container,
      force,
    ))
  if (force) {
    askConfirmation({
      title: '强制断开容器网络',
      message: `将强制断开 ${container} 与网络 ${network.name} 的连接。`,
      label: '强制断开',
      target: network.name,
      action: execute,
    })
    return
  }
  await execute()
  if (!errorMessage.value) dialog.value = null
}

function confirmDeleteNetwork(network: DockerNetwork) {
  askConfirmation({
    title: '删除网络',
    message: `将删除网络 ${network.name}。仍有容器连接时操作会失败。`,
    label: '删除网络',
    target: network.name,
    action: () => perform('网络已删除', () => deleteDockerNetwork(props.instance.id, network.id)),
  })
}

async function submitVolume() {
  if (!createVolumeForm.name.trim()) return
  await perform('存储卷已创建', () => createDockerVolume(
    props.instance.id,
    createVolumeForm.name.trim(),
    createVolumeForm.driver,
  ))
  if (!errorMessage.value) {
    dialog.value = null
    Object.assign(createVolumeForm, { name: '', driver: 'local' })
  }
}

function confirmDeleteVolume(volume: DockerVolume) {
  askConfirmation({
    title: '删除存储卷',
    message: `将永久删除 ${volume.name} 及其中的数据。`,
    label: '永久删除',
    target: volume.name,
    action: () => perform('存储卷已删除', () => deleteDockerVolume(props.instance.id, volume.name)),
  })
}

async function validateCompose() {
  const request = ++composeValidationRequest
  try {
    operationBusy.value = true
    errorMessage.value = ''
    const validation = await validateDockerCompose(props.instance.id, composeRequest())
    if (request === composeValidationRequest) {
      composeValidation.value = validation
      composeForm.confirmRisks = false
    }
  } catch (error) {
    if (request === composeValidationRequest) errorMessage.value = errorText(error)
  } finally {
    operationBusy.value = false
  }
}

async function deployCompose() {
  if (!composeValidation.value?.valid) return
  if (!composeValidation.value.config_digest) {
    errorMessage.value = '配置摘要缺失，请重新校验'
    composeValidation.value = null
    return
  }
  await perform('Compose 项目已部署', () => deployDockerCompose(props.instance.id, {
    ...composeRequest(),
    confirm_risks: composeForm.confirmRisks,
    config_digest: composeValidation.value?.config_digest,
  }))
  if (!errorMessage.value) {
    dialog.value = null
    resetComposeForm()
  }
}

function composeRequest(): DockerComposeRequest {
  const files = splitLines(composeForm.files)
  if (!files.length) throw new Error('至少需要一个 Compose YAML 路径')
  if (files.length > 8) throw new Error('Compose YAML 路径最多 8 个')
  return {
    project_name: composeForm.projectName.trim() || null,
    files,
    profiles: splitLines(composeForm.profiles),
    services: splitLines(composeForm.services),
  }
}

function openComposeAction(
  project: DockerComposeProject,
  action: 'pull' | 'up' | 'start' | 'stop' | 'restart' | 'down',
) {
  composeActionValidationRequest += 1
  composeActionValidationLoading.value = false
  Object.assign(composeAction, {
    project,
    action,
    profiles: '',
    services: '',
    removeVolumes: false,
    confirmRisks: false,
  })
  composeActionValidation.value = null
  dialog.value = 'compose-action'
}

async function validateComposeProjectAction() {
  const project = composeAction.project
  if (!project) return
  if (!project.config_files?.length) {
    errorMessage.value = 'Compose 项目缺少可校验的主机配置文件路径'
    return
  }
  const request = ++composeActionValidationRequest
  composeActionValidationLoading.value = true
  errorMessage.value = ''
  try {
    const validation = await validateDockerCompose(props.instance.id, {
      project_name: project.name,
      files: project.config_files,
      profiles: splitLines(composeAction.profiles),
      services: splitLines(composeAction.services),
    })
    if (request === composeActionValidationRequest) {
      composeActionValidation.value = validation
      composeAction.confirmRisks = false
    }
  } catch (error) {
    if (request === composeActionValidationRequest) errorMessage.value = errorText(error)
  } finally {
    if (request === composeActionValidationRequest) composeActionValidationLoading.value = false
  }
}

async function submitComposeAction() {
  const project = composeAction.project
  if (!project) return
  if (composeAction.action === 'up' && !composeActionValidation.value?.valid) {
    await validateComposeProjectAction()
    return
  }
  if (
    composeAction.action === 'up'
    && composeActionValidation.value?.warnings?.length
    && !composeAction.confirmRisks
  ) {
    errorMessage.value = '请先确认 Compose 配置中的高风险设置'
    return
  }
  if (composeAction.action === 'up' && !composeActionValidation.value?.config_digest) {
    errorMessage.value = '配置摘要缺失，请重新校验'
    composeActionValidation.value = null
    return
  }
  const execute = () => perform(`Compose ${composeAction.action} 已完成`, () => runDockerComposeAction(
    props.instance.id,
    project.name,
    composeAction.action,
    {
      profiles: splitLines(composeAction.profiles),
      services: splitLines(composeAction.services),
      config_digest: composeAction.action === 'up'
        ? composeActionValidation.value?.config_digest
        : undefined,
      remove_volumes: composeAction.removeVolumes,
      confirm_risks: composeAction.action === 'up'
        ? composeAction.confirmRisks
        : composeAction.action === 'down' && composeAction.removeVolumes,
    },
  ))
  if (composeAction.action === 'down' || composeAction.action === 'stop') {
    dialog.value = null
    askConfirmation({
      title: composeAction.action === 'down' ? '停止并移除 Compose 项目' : '停止 Compose 项目',
      message: composeAction.removeVolumes
        ? `将停止 ${project.name} 并永久删除项目存储卷。`
        : `将停止 ${project.name}${composeAction.action === 'down' ? ' 并移除项目容器与网络' : ''}。`,
      label: composeAction.action === 'down' ? '确认 Down' : '停止项目',
      target: project.name,
      action: execute,
    })
    return
  }
  await execute()
  if (!errorMessage.value) dialog.value = null
}

function confirmPrune(resource: 'containers' | 'images' | 'networks' | 'volumes') {
  const labels = { containers: '已停止容器', images: '未使用镜像', networks: '未使用网络', volumes: '未使用存储卷' }
  askConfirmation({
    title: `清理${labels[resource]}`,
    message: `将删除当前主机上的${labels[resource]}，此操作无法撤销。`,
    label: '确认清理',
    target: props.instance.name || props.instance.hostname,
    action: () => perform('清理已完成', () => pruneDockerResource(props.instance.id, resource)),
  })
}

function confirmSystemPrune(includeVolumes: boolean) {
  askConfirmation({
    title: 'Docker 系统清理',
    message: includeVolumes
      ? '将删除所有未使用的容器、镜像、网络和存储卷。存储卷数据无法恢复。'
      : '将删除所有未使用的容器、镜像和网络。',
    label: '确认系统清理',
    target: props.instance.name || props.instance.hostname,
    action: () => perform('系统清理已完成', () => pruneDockerSystem(props.instance.id, {
      all: true,
      volumes: includeVolumes,
      confirm: true,
    })),
  })
}

async function showDetails(kind: 'container' | 'image' | 'network' | 'volume' | 'project', item: unknown) {
  const request = ++detailRequest
  detailTitle.value = detailName(kind, item)
  detailData.value = null
  detailLoading.value = true
  try {
    let next: unknown
    if (kind === 'container') next = await getDockerContainer(props.instance.id, (item as DockerContainer).id)
    if (kind === 'image') next = await getDockerImage(props.instance.id, (item as DockerImage).id)
    if (kind === 'network') next = await getDockerNetwork(props.instance.id, (item as DockerNetwork).id)
    if (kind === 'volume') next = await getDockerVolume(props.instance.id, (item as DockerVolume).name)
    if (kind === 'project') next = await getDockerComposeProject(props.instance.id, (item as DockerComposeProject).name)
    if (request === detailRequest && manageable.value) detailData.value = next
  } catch (error) {
    if (request === detailRequest && manageable.value) detailData.value = { error: errorText(error) }
  } finally {
    if (request === detailRequest) detailLoading.value = false
  }
}

async function perform(message: string, action: () => Promise<unknown>) {
  operationBusy.value = true
  errorMessage.value = ''
  successMessage.value = ''
  try {
    const result = await action()
    const operation = dockerOperationResult(result)
    const detail = resultMessage(result) || message
    await refreshCurrent()
    if (operation?.partial_success === true) {
      errorMessage.value = `操作部分完成：${detail}`
      return false
    }
    if (operation?.completed === false) {
      errorMessage.value = `操作失败：${detail}`
      return false
    }
    successMessage.value = detail
    return true
  } catch (error) {
    errorMessage.value = errorText(error)
    return false
  } finally {
    operationBusy.value = false
  }
}

function askConfirmation(options: {
  title: string
  message: string
  label: string
  target: string
  action: () => Promise<unknown>
  danger?: boolean
  options?: 'generic' | 'container-delete'
}) {
  Object.assign(confirmDialog, {
    ...options,
    typed: '',
    danger: options.danger ?? true,
    options: options.options ?? 'generic',
  })
  dialog.value = 'confirm'
}

async function submitConfirmation() {
  if (!confirmDialog.action || confirmDialog.typed !== confirmDialog.target) return
  const action = confirmDialog.action
  dialog.value = null
  await action()
}

function openExec(container: DockerContainer) {
  terminalContainer.value = container
  selectedShell.value = '/bin/sh'
  dialog.value = 'exec-shell'
}

function startExec() {
  dialog.value = null
}

function resetContainerForm() {
  Object.assign(createContainerForm, {
    name: '', image: '', command: '', environment: '', ports: '', volumes: '', bindMounts: '',
    network: '', restartPolicy: 'unless-stopped', cpus: 0, memoryMb: 0,
    confirmReadWriteBindMounts: false,
  })
}

function resetComposeForm() {
  Object.assign(composeForm, {
    projectName: '', files: '', profiles: '', services: '', confirmRisks: false,
  })
  composeValidation.value = null
}

function splitLines(value: string) {
  return value.split(/\r?\n/).map((line) => line.trim()).filter(Boolean)
}

function parsePorts(value: string): DockerContainerCreateInput['ports'] {
  return splitLines(value).map((line) => {
    const [mapping, protocolValue = 'tcp'] = line.toLowerCase().split('/')
    if (protocolValue !== 'tcp' && protocolValue !== 'udp') throw new Error(`端口协议无效：${line}`)
    const parts = mapping.split(':')
    if (parts.length < 1 || parts.length > 3) throw new Error(`端口映射格式无效：${line}`)
    const containerPort = Number(parts.at(-1))
    const hostPort = parts.length >= 2 ? Number(parts.at(-2)) : null
    if (!validPort(containerPort) || (hostPort !== null && !validPort(hostPort))) {
      throw new Error(`端口范围无效：${line}`)
    }
    return {
      container_port: containerPort,
      host_port: hostPort,
      host_ip: parts.length === 3 ? parts[0] : null,
      protocol: protocolValue,
    }
  })
}

function parseMounts(value: string, bind: boolean) {
  return splitLines(value).map((line) => {
    const parts = line.split(':')
    const mode = parts.at(-1)?.toLowerCase()
    const hasMode = mode === 'ro' || mode === 'rw'
    const targetIndex = hasMode ? parts.length - 2 : parts.length - 1
    const source = parts.slice(0, targetIndex).join(':').trim()
    const target = parts[targetIndex]?.trim() || ''
    if (!source || !target.startsWith('/')) {
      throw new Error(`${bind ? 'Bind mount' : '存储卷'}格式无效：${line}`)
    }
    return { source, target, readonly: mode === 'ro' }
  })
}

function validPort(value: number) {
  return Number.isInteger(value) && value >= 1 && value <= 65535
}

function normalizedState(container: DockerContainer) {
  return (container.state || container.status || 'unknown').split(' ')[0].toLowerCase()
}

function containerName(container: DockerContainer) {
  return container.name || container.names?.[0]?.replace(/^\//, '') || shortId(container.id)
}

function imageLabel(image: DockerImage) {
  return image.repo_tags?.find((tag) => tag !== '<none>:<none>') || shortId(image.id)
}

function imageTags(image: DockerImage) {
  const tags = image.repo_tags?.filter((tag) => tag !== '<none>:<none>') || []
  return tags.length ? tags.join(', ') : '<none>'
}

function containerPorts(container: DockerContainer) {
  if (container.ports_text) return container.ports_text
  return (container.ports || []).map((port) =>
    `${port.public_port ?? port.host_port ?? ''}${port.public_port || port.host_port ? ':' : ''}${port.private_port ?? port.container_port ?? ''}/${port.type || 'tcp'}`,
  ).join(', ') || '—'
}

function shortId(id: string) {
  return id.replace(/^sha256:/, '').slice(0, 12)
}

function formatDockerTime(value: string | number | null | undefined) {
  if (typeof value === 'number') return formatTime(value)
  if (!value) return '未知'
  const timestamp = Date.parse(value)
  return Number.isNaN(timestamp) ? value : new Date(timestamp).toLocaleString()
}

function statusLabel(status: DockerStatus['status']) {
  return {
    unknown: 'Docker 状态尚未上报',
    not_installed: '此实例未安装 Docker',
    daemon_unreachable: 'Docker daemon 不可达',
    permission_denied: 'Agent 服务账号无权访问 Docker',
    unsupported_version: 'Docker 版本低于最低支持版本 20.10',
    ready: 'Docker 可管理',
    error: 'Docker 检测失败',
  }[status]
}

function stateLabel(state: string) {
  return ({ running: '运行中', exited: '已退出', paused: '已暂停', created: '已创建', restarting: '重启中' } as Record<string, string>)[state] || state
}

function detailName(kind: string, item: unknown) {
  if (kind === 'container') return `${containerName(item as DockerContainer)} · 容器详情`
  if (kind === 'image') return `${imageLabel(item as DockerImage)} · 镜像详情`
  if (kind === 'network') return `${(item as DockerNetwork).name} · 网络详情`
  if (kind === 'volume') return `${(item as DockerVolume).name} · 存储卷详情`
  return `${(item as DockerComposeProject).name} · Compose 详情`
}

function redactedJson(value: unknown) {
  const redact = (input: unknown, key = ''): unknown => {
    if (Array.isArray(input)) {
      if (key.toLowerCase() === 'env' || key.toLowerCase() === 'environment') {
        return input.map((entry) => typeof entry === 'string'
          ? `${entry.split('=', 1)[0]}=••••••`
          : '••••••')
      }
      return input.map((entry) => redact(entry))
    }
    if (input && typeof input === 'object') {
      return Object.fromEntries(Object.entries(input).map(([childKey, child]) => {
        const normalized = childKey.toLowerCase()
        if (/password|secret|token|credential|auth/.test(normalized)) return [childKey, '••••••']
        return [childKey, redact(child, childKey)]
      }))
    }
    return input
  }
  return JSON.stringify(redact(value), null, 2)
}

function resultMessage(value: unknown) {
  const result = dockerOperationResult(value)
  if (!result) return ''
  let message = ''
  if (typeof result.succeeded_stages === 'number' && typeof result.failed_stages === 'number') {
    if (result.failed_stages === 0) {
      message = `已完成 ${result.succeeded_stages} 个清理阶段`
    } else if (result.succeeded_stages === 0) {
      message = `${result.failed_stages} 个清理阶段均失败`
    } else {
      message = `已完成 ${result.succeeded_stages} 个清理阶段，${result.failed_stages} 个阶段失败`
    }
  } else if (typeof result.message === 'string') {
    message = result.message
  }
  const reclaimed = result.reclaimed_bytes ?? result.space_reclaimed
  if (!message && typeof reclaimed === 'number') message = `已回收 ${formatBytes(reclaimed)}`
  const truncated = result.output_truncated === true
    || result.resources?.some((stage) => stage.output_truncated === true)
  if (truncated) {
    message = `${message ? `${message}；` : ''}输出已截断，仅保留末尾`
  }
  return message
}

function dockerOperationResult(value: unknown): DockerOperationResult | null {
  return value && typeof value === 'object' ? value as DockerOperationResult : null
}

function errorText(error: unknown) {
  return error instanceof Error ? error.message : 'Docker 操作失败'
}

async function mapLimit<T, R>(items: T[], limit: number, mapper: (item: T) => Promise<R>) {
  const results = new Array<R>(items.length)
  let index = 0
  await Promise.all(Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (index < items.length) {
      const current = index
      index += 1
      results[current] = await mapper(items[current])
    }
  }))
  return results
}
</script>

<template>
  <div class="docker-manager">
    <header class="docker-toolbar">
      <nav class="docker-view-tabs" aria-label="Docker 资源">
        <button :class="{ active: view === 'containers' }" type="button" @click="view = 'containers'"><Boxes :size="14" />容器</button>
        <button :class="{ active: view === 'images' }" type="button" @click="view = 'images'"><Images :size="14" />镜像</button>
        <button :class="{ active: view === 'networks' }" type="button" @click="view = 'networks'"><Network :size="14" />网络</button>
        <button :class="{ active: view === 'volumes' }" type="button" @click="view = 'volumes'"><Database :size="14" />存储卷</button>
        <button :class="{ active: view === 'compose' }" type="button" :disabled="!composeAvailable" :title="composeAvailable ? 'Compose' : '未安装 Docker Compose v2'" @click="view = 'compose'"><Archive :size="14" />Compose</button>
        <button :class="{ active: view === 'system' }" type="button" @click="view = 'system'"><CircleGauge :size="14" />系统</button>
      </nav>
      <div class="docker-toolbar-actions">
        <label v-if="view !== 'system'" class="docker-search">
          <Search :size="14" />
          <input v-model="search" type="search" placeholder="搜索" aria-label="搜索 Docker 资源" />
        </label>
        <button
          v-if="view !== 'system'"
          class="text-button docker-create-button"
          type="button"
          :disabled="!manageable || (view === 'compose' && !composeAvailable)"
          @click="openCreateForCurrentView"
        >
          <Plus :size="14" />{{ view === 'images' ? '拉取' : view === 'compose' ? '部署' : '创建' }}
        </button>
        <button class="icon-button" type="button" title="刷新" :disabled="loading || !manageable" @click="refreshCurrent">
          <RefreshCw :class="{ spin: loading }" :size="15" />
        </button>
      </div>
    </header>

    <section v-if="!manageable" class="docker-diagnostic">
      <ShieldAlert :size="30" />
      <strong>{{ statusLabel(localStatus.status) }}</strong>
      <p>{{ dockerDiagnostic }}</p>
      <dl>
        <div><dt>CLI</dt><dd>{{ localStatus.cli_version || '未知' }}</dd></div>
        <div><dt>Engine</dt><dd>{{ localStatus.engine_version || '未知' }}</dd></div>
        <div><dt>API</dt><dd>{{ localStatus.api_version || '未知' }}</dd></div>
        <div><dt>Compose</dt><dd>{{ localStatus.compose_version || '不可用' }}</dd></div>
        <div><dt>检查时间</dt><dd>{{ formatTime(localStatus.checked_at) }}</dd></div>
      </dl>
    </section>

    <template v-else>
      <div v-if="errorMessage || successMessage" :class="['docker-notice', { error: errorMessage }]" role="status">
        <AlertTriangle v-if="errorMessage" :size="14" />
        <span>{{ errorMessage || successMessage }}</span>
        <button type="button" title="关闭提示" @click="errorMessage = ''; successMessage = ''"><X :size="13" /></button>
      </div>

      <div v-if="view === 'containers'" class="docker-resource-view">
        <div class="docker-subtoolbar">
          <div class="docker-state-filter" role="group" aria-label="容器状态">
            <button :class="{ active: containerState === 'all' }" type="button" @click="containerState = 'all'">全部 {{ containers.length }}</button>
            <button :class="{ active: containerState === 'running' }" type="button" @click="containerState = 'running'">运行中 {{ containers.filter((item) => normalizedState(item) === 'running').length }}</button>
            <button :class="{ active: containerState === 'exited' }" type="button" @click="containerState = 'exited'">已退出 {{ containers.filter((item) => normalizedState(item) === 'exited').length }}</button>
          </div>
          <button class="text-button danger" type="button" :disabled="operationBusy" @click="confirmPrune('containers')"><Trash2 :size="13" />清理已停止项</button>
        </div>
        <div class="docker-table-shell">
          <table class="docker-table docker-container-table">
            <thead><tr><th>容器</th><th>镜像</th><th>状态</th><th>CPU</th><th>内存</th><th>端口</th><th>操作</th></tr></thead>
            <tbody>
              <tr v-for="container in filteredContainers" :key="container.id">
                <td><button class="docker-resource-name" type="button" @click="showDetails('container', container)"><Box :size="15" /><span><strong>{{ containerName(container) }}</strong><small>{{ shortId(container.id) }}</small></span></button></td>
                <td class="docker-ellipsis" :title="container.image">{{ container.image }}</td>
                <td><span :class="['docker-state', normalizedState(container)]">{{ stateLabel(normalizedState(container)) }}</span><small class="docker-cell-detail">{{ container.status }}</small></td>
                <td>{{ containerStats[container.id]?.cpu_percent == null ? '—' : `${containerStats[container.id].cpu_percent?.toFixed(1)}%` }}</td>
                <td>{{ formatBytes(containerStats[container.id]?.memory_usage) }}<small v-if="containerStats[container.id]?.memory_limit" class="docker-cell-detail">/ {{ formatBytes(containerStats[container.id]?.memory_limit) }}</small></td>
                <td class="docker-port-list">{{ containerPorts(container) }}</td>
                <td><div class="docker-row-actions">
                  <button v-if="normalizedState(container) !== 'running'" class="icon-button subtle" type="button" title="启动" :disabled="operationBusy" @click="containerAction(container, 'start')"><Play :size="14" /></button>
                  <button v-else class="icon-button subtle" type="button" title="停止" :disabled="operationBusy" @click="containerAction(container, 'stop')"><Square :size="13" /></button>
                  <button class="icon-button subtle" type="button" title="重启" :disabled="operationBusy || normalizedState(container) !== 'running'" @click="containerAction(container, 'restart')"><RotateCw :size="14" /></button>
                  <button v-if="normalizedState(container) !== 'paused'" class="icon-button subtle" type="button" title="暂停" :disabled="operationBusy || normalizedState(container) !== 'running'" @click="containerAction(container, 'pause')"><Pause :size="14" /></button>
                  <button v-else class="icon-button subtle" type="button" title="恢复" :disabled="operationBusy" @click="containerAction(container, 'unpause')"><Play :size="14" /></button>
                  <button class="icon-button subtle" type="button" title="日志" @click="logContainer = container"><ScrollText :size="14" /></button>
                  <button class="icon-button subtle" type="button" title="终端" :disabled="normalizedState(container) !== 'running'" @click="openExec(container)"><Terminal :size="14" /></button>
                  <button class="icon-button subtle" type="button" title="重命名" :disabled="operationBusy" @click="openRenameContainer(container)"><Pencil :size="14" /></button>
                  <button class="icon-button subtle danger" type="button" title="终止" :disabled="operationBusy || normalizedState(container) !== 'running'" @click="containerAction(container, 'kill')"><Skull :size="14" /></button>
                  <button class="icon-button subtle danger" type="button" title="删除" :disabled="operationBusy" @click="confirmDeleteContainer(container)"><Trash2 :size="14" /></button>
                </div></td>
              </tr>
            </tbody>
          </table>
          <div v-if="loading" class="docker-table-state"><LoaderCircle class="spin" :size="18" />正在读取容器</div>
          <div v-else-if="!filteredContainers.length" class="docker-table-state"><PackageOpen :size="22" />没有匹配的容器</div>
        </div>
      </div>

      <div v-else-if="view === 'images'" class="docker-resource-view">
        <div class="docker-subtoolbar"><span>{{ filteredImages.length }} 个镜像</span><button class="text-button danger" type="button" :disabled="operationBusy" @click="confirmPrune('images')"><Trash2 :size="13" />清理未使用项</button></div>
        <div class="docker-table-shell">
          <table class="docker-table docker-image-table"><thead><tr><th>标签</th><th>镜像 ID</th><th>大小</th><th>创建时间</th><th>操作</th></tr></thead><tbody>
            <tr v-for="image in filteredImages" :key="image.id">
              <td><button class="docker-resource-name" type="button" @click="showDetails('image', image)"><Images :size="15" /><span><strong>{{ imageTags(image) }}</strong><small>{{ image.repo_digests?.[0] || '无摘要' }}</small></span></button></td>
              <td><code>{{ shortId(image.id) }}</code></td><td>{{ formatBytes(image.size ?? image.virtual_size) }}</td><td>{{ formatDockerTime(image.created) }}</td>
              <td><div class="docker-row-actions"><button class="icon-button subtle" type="button" title="查看详情" @click="showDetails('image', image)"><Eye :size="14" /></button><button class="icon-button subtle" type="button" title="添加标签" @click="openTagImage(image)"><Tag :size="14" /></button><button class="icon-button subtle danger" type="button" title="删除镜像" :disabled="operationBusy" @click="confirmDeleteImage(image)"><Trash2 :size="14" /></button></div></td>
            </tr>
          </tbody></table>
          <div v-if="loading" class="docker-table-state"><LoaderCircle class="spin" :size="18" />正在读取镜像</div><div v-else-if="!filteredImages.length" class="docker-table-state"><PackageOpen :size="22" />没有匹配的镜像</div>
        </div>
      </div>

      <div v-else-if="view === 'networks'" class="docker-resource-view">
        <div class="docker-subtoolbar"><span>{{ filteredNetworks.length }} 个网络</span><button class="text-button danger" type="button" :disabled="operationBusy" @click="confirmPrune('networks')"><Trash2 :size="13" />清理未使用项</button></div>
        <div class="docker-table-shell"><table class="docker-table"><thead><tr><th>名称</th><th>ID</th><th>驱动</th><th>作用域</th><th>属性</th><th>操作</th></tr></thead><tbody>
          <tr v-for="network in filteredNetworks" :key="network.id"><td><button class="docker-resource-name" type="button" @click="showDetails('network', network)"><Network :size="15" /><span><strong>{{ network.name }}</strong></span></button></td><td><code>{{ shortId(network.id) }}</code></td><td>{{ network.driver }}</td><td>{{ network.scope || 'local' }}</td><td>{{ [network.internal ? '内部' : '', network.attachable ? '可连接' : ''].filter(Boolean).join(' · ') || '标准' }}</td><td><div class="docker-row-actions"><button class="icon-button subtle" type="button" title="连接容器" @click="openNetworkMembership(network, 'connect')"><Link2 :size="14" /></button><button class="icon-button subtle" type="button" title="断开容器" @click="openNetworkMembership(network, 'disconnect')"><Unlink :size="14" /></button><button class="icon-button subtle danger" type="button" title="删除网络" :disabled="operationBusy || ['bridge', 'host', 'none'].includes(network.name)" @click="confirmDeleteNetwork(network)"><Trash2 :size="14" /></button></div></td></tr>
        </tbody></table><div v-if="loading" class="docker-table-state"><LoaderCircle class="spin" :size="18" />正在读取网络</div><div v-else-if="!filteredNetworks.length" class="docker-table-state"><PackageOpen :size="22" />没有匹配的网络</div></div>
      </div>

      <div v-else-if="view === 'volumes'" class="docker-resource-view">
        <div class="docker-subtoolbar"><span>{{ filteredVolumes.length }} 个存储卷</span><button class="text-button danger" type="button" :disabled="operationBusy" @click="confirmPrune('volumes')"><Trash2 :size="13" />清理未使用项</button></div>
        <div class="docker-table-shell"><table class="docker-table"><thead><tr><th>名称</th><th>驱动</th><th>挂载点</th><th>引用</th><th>大小</th><th>操作</th></tr></thead><tbody>
          <tr v-for="volume in filteredVolumes" :key="volume.name"><td><button class="docker-resource-name" type="button" @click="showDetails('volume', volume)"><Database :size="15" /><span><strong>{{ volume.name }}</strong></span></button></td><td>{{ volume.driver }}</td><td class="docker-ellipsis" :title="volume.mountpoint">{{ volume.mountpoint || '—' }}</td><td>{{ volume.usage_data?.ref_count ?? '—' }}</td><td>{{ formatBytes(volume.usage_data?.size) }}</td><td><div class="docker-row-actions"><button class="icon-button subtle" type="button" title="查看详情" @click="showDetails('volume', volume)"><Eye :size="14" /></button><button class="icon-button subtle danger" type="button" title="删除存储卷" :disabled="operationBusy" @click="confirmDeleteVolume(volume)"><Trash2 :size="14" /></button></div></td></tr>
        </tbody></table><div v-if="loading" class="docker-table-state"><LoaderCircle class="spin" :size="18" />正在读取存储卷</div><div v-else-if="!filteredVolumes.length" class="docker-table-state"><PackageOpen :size="22" />没有匹配的存储卷</div></div>
      </div>

      <div v-else-if="view === 'compose'" class="docker-resource-view">
        <div v-if="!composeAvailable" class="docker-inline-warning"><AlertTriangle :size="15" />当前主机未安装 Docker Compose v2，Compose 操作不可用。</div>
        <div class="docker-subtoolbar"><span>{{ filteredProjects.length }} 个项目</span><span>Compose {{ localStatus.compose_version || '不可用' }}</span></div>
        <div class="docker-table-shell"><table class="docker-table docker-compose-table"><thead><tr><th>项目</th><th>状态</th><th>服务</th><th>容器</th><th>配置文件</th><th>操作</th></tr></thead><tbody>
          <tr v-for="project in filteredProjects" :key="project.name"><td><button class="docker-resource-name" type="button" @click="showDetails('project', project)"><Archive :size="15" /><span><strong>{{ project.name }}</strong><small>{{ project.working_dir || '—' }}</small></span></button></td><td><span class="docker-state running">{{ project.status || '已发现' }}</span></td><td>{{ project.services?.length ?? '—' }}</td><td>{{ project.running ?? '—' }} / {{ project.containers ?? '—' }}</td><td class="docker-ellipsis" :title="project.config_files?.join(', ')">{{ project.config_files?.join(', ') || '—' }}</td><td><div class="docker-row-actions"><button class="icon-button subtle" type="button" title="Pull" :disabled="operationBusy" @click="openComposeAction(project, 'pull')"><Archive :size="14" /></button><button class="icon-button subtle" type="button" title="Up" :disabled="operationBusy" @click="openComposeAction(project, 'up')"><Boxes :size="14" /></button><button class="icon-button subtle" type="button" title="启动" :disabled="operationBusy" @click="openComposeAction(project, 'start')"><Play :size="14" /></button><button class="icon-button subtle" type="button" title="重启" :disabled="operationBusy" @click="openComposeAction(project, 'restart')"><RotateCw :size="14" /></button><button class="icon-button subtle" type="button" title="停止" :disabled="operationBusy" @click="openComposeAction(project, 'stop')"><Square :size="13" /></button><button class="icon-button subtle danger" type="button" title="Down" :disabled="operationBusy" @click="openComposeAction(project, 'down')"><Trash2 :size="14" /></button></div></td></tr>
        </tbody></table><div v-if="loading" class="docker-table-state"><LoaderCircle class="spin" :size="18" />正在读取 Compose 项目</div><div v-else-if="!filteredProjects.length" class="docker-table-state"><PackageOpen :size="22" />没有匹配的 Compose 项目</div></div>
      </div>

      <div v-else class="docker-system-view">
        <header><div><Server :size="17" /><div><strong>Docker Engine {{ localStatus.engine_version || '未知' }}</strong><span>API {{ localStatus.api_version || '未知' }} · CLI {{ localStatus.cli_version || '未知' }}</span></div></div><button class="text-button" type="button" :disabled="loading" @click="loadDiskUsage"><RefreshCw :size="14" />刷新空间统计</button></header>
        <div class="docker-stat-grid">
          <div><Images :size="17" /><span>镜像</span><strong>{{ diskUsage?.image_count ?? diskUsage?.images?.length ?? '—' }}</strong><small>{{ formatBytes(diskUsage?.images_size ?? diskUsage?.layers_size) }}</small></div>
          <div><Boxes :size="17" /><span>容器</span><strong>{{ diskUsage?.container_count ?? diskUsage?.containers?.length ?? '—' }}</strong><small>{{ formatBytes(diskUsage?.containers_size) }}</small></div>
          <div><Database :size="17" /><span>存储卷</span><strong>{{ diskUsage?.volume_count ?? diskUsage?.volumes?.length ?? '—' }}</strong><small>{{ formatBytes(diskUsage?.volumes_size) }}</small></div>
          <div><HardDrive :size="17" /><span>可回收</span><strong>{{ formatBytes(diskUsage?.reclaimable_size) }}</strong><small>未使用资源</small></div>
        </div>
        <section class="docker-prune-zone"><header><div><ShieldAlert :size="17" /><div><strong>系统清理</strong><span>清理未使用的 Docker 资源</span></div></div></header><div><button class="text-button danger" type="button" :disabled="operationBusy" @click="confirmSystemPrune(false)"><Trash2 :size="14" />清理容器、镜像和网络</button><button class="text-button danger" type="button" :disabled="operationBusy" @click="confirmSystemPrune(true)"><Database :size="14" />同时清理存储卷</button></div></section>
      </div>
    </template>

    <aside v-if="detailTitle" class="docker-detail-drawer" aria-label="Docker 资源详情">
      <header><strong>{{ detailTitle }}</strong><button class="icon-button subtle" type="button" title="关闭详情" @click="closeDetails"><X :size="15" /></button></header>
      <div v-if="detailLoading" class="docker-table-state"><LoaderCircle class="spin" :size="18" />正在读取详情</div>
      <pre v-else>{{ redactedJson(detailData) }}</pre>
    </aside>

    <div v-if="dialog" class="docker-dialog-backdrop" @click.self="dialog = null">
      <form v-if="dialog === 'create-container'" class="docker-dialog docker-dialog-wide" @submit.prevent="submitContainer">
        <header><div><Box :size="18" /><h3>创建容器</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header>
        <div class="docker-form-grid"><label><span>容器名称</span><input v-model="createContainerForm.name" autocomplete="off" /></label><label><span>镜像</span><input v-model="createContainerForm.image" required placeholder="nginx:latest" /></label><label class="docker-form-full"><span>命令参数</span><textarea v-model="createContainerForm.command" rows="3" placeholder="每行一个 argv 参数"></textarea></label><label class="docker-form-full"><span>环境变量</span><textarea v-model="createContainerForm.environment" rows="3" placeholder="KEY=value，每行一项"></textarea></label><label><span>端口映射</span><textarea v-model="createContainerForm.ports" rows="3" placeholder="127.0.0.1:8080:80/tcp"></textarea></label><label><span>命名卷</span><textarea v-model="createContainerForm.volumes" rows="3" placeholder="data:/var/lib/app:rw"></textarea></label><label><span>Bind mount</span><textarea v-model="createContainerForm.bindMounts" rows="3" placeholder="/host/path:/app/data:ro"></textarea></label><label><span>初始网络</span><input v-model="createContainerForm.network" placeholder="bridge" /></label><label><span>重启策略</span><select v-model="createContainerForm.restartPolicy"><option value="no">不自动重启</option><option value="always">总是</option><option value="unless-stopped">除非手动停止</option><option value="on-failure">失败时</option></select></label><label><span>CPU 限制</span><input v-model.number="createContainerForm.cpus" type="number" min="0" step="0.1" /></label><label><span>内存限制 (MiB)</span><input v-model.number="createContainerForm.memoryMb" type="number" min="0" step="1" /></label></div>
        <label class="docker-checkbox"><input v-model="createContainerForm.confirmReadWriteBindMounts" type="checkbox" /><span>确认允许配置中的读写 Bind mount</span></label>
        <div v-if="errorMessage" class="docker-form-error">{{ errorMessage }}</div><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy"><LoaderCircle v-if="operationBusy" class="spin" :size="14" /><Plus v-else :size="14" />创建容器</button></footer>
      </form>

      <form v-else-if="dialog === 'pull-image'" class="docker-dialog" @submit.prevent="submitPullImage"><header><div><Images :size="18" /><h3>拉取镜像</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header><label><span>镜像引用</span><input v-model="pullImageReference" required placeholder="registry.example.com/team/app:tag" /></label><div v-if="errorMessage" class="docker-form-error">{{ errorMessage }}</div><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy"><Archive :size="14" />拉取</button></footer></form>

      <form v-else-if="dialog === 'tag-image'" class="docker-dialog" @submit.prevent="submitTagImage"><header><div><Tag :size="18" /><h3>添加镜像标签</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header><label><span>仓库</span><input v-model="tagImageForm.repository" required /></label><label><span>标签</span><input v-model="tagImageForm.tag" required /></label><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy"><Tag :size="14" />保存标签</button></footer></form>

      <form v-else-if="dialog === 'rename-container'" class="docker-dialog" @submit.prevent="submitRenameContainer"><header><div><Pencil :size="18" /><h3>重命名容器</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header><p class="docker-dialog-target">{{ renameContainerTarget ? containerName(renameContainerTarget) : '' }}</p><label><span>新名称</span><input v-model="renameContainerName" required autocomplete="off" /></label><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy"><Pencil :size="14" />保存名称</button></footer></form>

      <form v-else-if="dialog === 'create-network'" class="docker-dialog" @submit.prevent="submitNetwork"><header><div><Network :size="18" /><h3>创建网络</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header><label><span>名称</span><input v-model="createNetworkForm.name" required /></label><label><span>驱动</span><select v-model="createNetworkForm.driver"><option value="bridge">bridge</option><option value="macvlan">macvlan</option><option value="ipvlan">ipvlan</option></select></label><label class="docker-checkbox"><input v-model="createNetworkForm.internal" type="checkbox" /><span>内部网络</span></label><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy"><Plus :size="14" />创建网络</button></footer></form>

      <form v-else-if="dialog === 'network-membership'" class="docker-dialog" @submit.prevent="submitNetworkMembership"><header><div><Link2 :size="18" /><h3>{{ networkMembership.mode === 'connect' ? '连接容器' : '断开容器' }}</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header><p class="docker-dialog-target">{{ networkMembership.network?.name }}</p><label><span>容器 ID 或名称</span><input v-model="networkMembership.container" required /></label><label v-if="networkMembership.mode === 'connect'"><span>网络别名</span><textarea v-model="networkMembership.aliases" rows="3" placeholder="每行一个别名"></textarea></label><label v-else class="docker-checkbox"><input v-model="networkMembership.force" type="checkbox" /><span>强制断开</span></label><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy">{{ networkMembership.mode === 'connect' ? '连接' : '断开' }}</button></footer></form>

      <form v-else-if="dialog === 'create-volume'" class="docker-dialog" @submit.prevent="submitVolume"><header><div><Database :size="18" /><h3>创建存储卷</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header><label><span>名称</span><input v-model="createVolumeForm.name" required /></label><label><span>驱动</span><input v-model="createVolumeForm.driver" required /></label><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy"><Plus :size="14" />创建存储卷</button></footer></form>

      <form v-else-if="dialog === 'compose-deploy'" class="docker-dialog docker-dialog-wide" @submit.prevent="composeValidation?.valid ? deployCompose() : validateCompose()">
        <header><div><Archive :size="18" /><h3>部署 Compose 项目</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header>
        <div class="docker-form-grid"><label><span>项目名称</span><input v-model="composeForm.projectName" /></label><label class="docker-form-full"><span>主机 YAML 路径</span><textarea v-model="composeForm.files" required rows="4" placeholder="每行一个路径，最多 8 个"></textarea></label><label><span>Profiles</span><textarea v-model="composeForm.profiles" rows="3" placeholder="每行一项"></textarea></label><label><span>Services</span><textarea v-model="composeForm.services" rows="3" placeholder="留空部署全部服务"></textarea></label></div>
        <DockerComposeValidationPreview v-if="composeValidation" :validation="composeValidation" />
        <label v-if="composeValidation?.warnings.length" class="docker-checkbox"><input v-model="composeForm.confirmRisks" type="checkbox" /><span>确认接受上述高风险配置</span></label>
        <div v-if="errorMessage" class="docker-form-error">{{ errorMessage }}</div>
        <footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy || Boolean(composeValidation?.warnings.length && !composeForm.confirmRisks)"><LoaderCircle v-if="operationBusy" class="spin" :size="14" /><ShieldAlert v-else-if="!composeValidation?.valid" :size="14" /><Play v-else :size="14" />{{ composeValidation?.valid ? '部署' : '校验配置' }}</button></footer>
      </form>

      <form v-else-if="dialog === 'compose-action'" :class="['docker-dialog', { 'docker-dialog-wide': composeActionValidation }]" @submit.prevent="submitComposeAction">
        <header><div><Archive :size="18" /><h3>Compose {{ composeAction.action }}</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header>
        <p class="docker-dialog-target">{{ composeAction.project?.name }}</p>
        <label><span>Services</span><textarea v-model="composeAction.services" rows="3" placeholder="留空处理全部服务"></textarea></label>
        <label v-if="composeAction.action === 'up' || composeAction.action === 'pull'"><span>Profiles</span><textarea v-model="composeAction.profiles" rows="3" placeholder="每行一项"></textarea></label>
        <DockerComposeValidationPreview v-if="composeAction.action === 'up' && composeActionValidation" :validation="composeActionValidation" />
        <label v-if="composeAction.action === 'up' && composeActionValidation?.warnings.length" class="docker-checkbox danger"><input v-model="composeAction.confirmRisks" type="checkbox" /><span>确认接受上述高风险配置</span></label>
        <label v-if="composeAction.action === 'down'" class="docker-checkbox danger"><input v-model="composeAction.removeVolumes" type="checkbox" /><span>同时永久删除项目存储卷</span></label>
        <div v-if="errorMessage" class="docker-form-error">{{ errorMessage }}</div>
        <footer><button class="text-button" type="button" @click="dialog = null">取消</button><button class="primary-button" type="submit" :disabled="operationBusy || composeActionValidationLoading || Boolean(composeAction.action === 'up' && composeActionValidation?.warnings.length && !composeAction.confirmRisks)"><LoaderCircle v-if="composeActionValidationLoading" class="spin" :size="14" />{{ composeAction.action === 'up' && !composeActionValidation?.valid ? '校验配置' : `执行 ${composeAction.action}` }}</button></footer>
      </form>

      <form v-else-if="dialog === 'exec-shell'" class="docker-dialog" @submit.prevent="startExec"><header><div><Terminal :size="18" /><h3>打开容器终端</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null; terminalContainer = null"><X :size="15" /></button></header><p class="docker-dialog-target">{{ terminalContainer ? containerName(terminalContainer) : '' }}</p><label><span>Shell</span><select v-model="selectedShell"><option value="/bin/sh">/bin/sh</option><option value="/bin/bash">/bin/bash</option><option value="/bin/ash">/bin/ash</option></select></label><footer><button class="text-button" type="button" @click="dialog = null; terminalContainer = null">取消</button><button class="primary-button" type="submit"><Terminal :size="14" />连接</button></footer></form>

      <form v-else class="docker-dialog docker-confirm-dialog" @submit.prevent="submitConfirmation"><header><div><ShieldAlert :size="18" /><h3>{{ confirmDialog.title }}</h3></div><button class="icon-button subtle" type="button" title="关闭" @click="dialog = null"><X :size="15" /></button></header><p>{{ confirmDialog.message }}</p><template v-if="confirmDialog.options === 'container-delete'"><label class="docker-checkbox"><input v-model="deleteContainerOptions.force" type="checkbox" /><span>强制停止运行中的容器</span></label><label class="docker-checkbox danger"><input v-model="deleteContainerOptions.removeVolumes" type="checkbox" /><span>同时删除匿名存储卷</span></label></template><label><span>输入目标名称以确认</span><input v-model="confirmDialog.typed" required autocomplete="off" :placeholder="confirmDialog.target" /></label><footer><button class="text-button" type="button" @click="dialog = null">取消</button><button :class="['primary-button', { 'docker-danger-button': confirmDialog.danger }]" type="submit" :disabled="confirmDialog.typed !== confirmDialog.target || operationBusy"><Trash2 :size="14" />{{ confirmDialog.label }}</button></footer></form>
    </div>

    <DockerLogsPanel v-if="logContainer" :instance-id="instance.id" :container-id="logContainer.id" :container-name="containerName(logContainer)" @close="logContainer = null" />
    <DockerExecTerminal v-if="terminalContainer && !dialog" :instance-id="instance.id" :container-id="terminalContainer.id" :container-name="containerName(terminalContainer)" :shell="selectedShell" @close="terminalContainer = null" />
  </div>
</template>
