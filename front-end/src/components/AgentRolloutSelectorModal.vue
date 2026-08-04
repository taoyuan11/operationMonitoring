<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { Check, LoaderCircle, Search, Server, ShieldAlert, Wifi, WifiOff, X } from 'lucide-vue-next'
import type { AgentRelease, AgentRolloutCandidate } from '../types/domain'

const props = defineProps<{
  release: AgentRelease
  candidates: AgentRolloutCandidate[]
  loading: boolean
  mode: 'publish' | 'add'
}>()

const emit = defineEmits<{
  close: []
  confirm: [instanceIds: string[]]
}>()

const search = ref('')
const statusFilter = ref<'all' | 'online' | 'offline'>('all')
const platformFilter = ref('all')
const selectedIds = ref<string[]>([])

const platforms = computed(() => [...new Set(
  props.candidates
    .map((candidate) => `${candidate.os}/${candidate.native_arch}`)
    .filter((platform) => !platform.endsWith('/')),
)].sort())

const filteredCandidates = computed(() => {
  const query = search.value.trim().toLocaleLowerCase()
  return props.candidates.filter((candidate) => {
    if (statusFilter.value === 'online' && !candidate.online) return false
    if (statusFilter.value === 'offline' && candidate.online) return false
    if (
      platformFilter.value !== 'all'
      && `${candidate.os}/${candidate.native_arch}` !== platformFilter.value
    ) return false
    if (!query) return true
    return [candidate.name, candidate.hostname, candidate.instance_id, candidate.agent_version]
      .some((value) => value.toLocaleLowerCase().includes(query))
  })
})

const selectableCandidates = computed(() => filteredCandidates.value.filter(isSelectable))
const allVisibleSelected = computed(() => (
  selectableCandidates.value.length > 0
  && selectableCandidates.value.every((candidate) => selectedIds.value.includes(candidate.instance_id))
))
const someVisibleSelected = computed(() => (
  !allVisibleSelected.value
  && selectableCandidates.value.some((candidate) => selectedIds.value.includes(candidate.instance_id))
))

watch(
  () => props.candidates,
  (candidates) => {
    const selectable = new Set(candidates.filter(isSelectable).map((candidate) => candidate.instance_id))
    selectedIds.value = selectedIds.value.filter((instanceId) => selectable.has(instanceId))
  },
)

function isSelectable(candidate: AgentRolloutCandidate) {
  return candidate.eligible && !candidate.selected
}

function toggleCandidate(candidate: AgentRolloutCandidate) {
  if (!isSelectable(candidate)) return
  selectedIds.value = selectedIds.value.includes(candidate.instance_id)
    ? selectedIds.value.filter((instanceId) => instanceId !== candidate.instance_id)
    : [...selectedIds.value, candidate.instance_id]
}

function toggleVisible() {
  const visibleIds = selectableCandidates.value.map((candidate) => candidate.instance_id)
  if (allVisibleSelected.value) {
    const visible = new Set(visibleIds)
    selectedIds.value = selectedIds.value.filter((instanceId) => !visible.has(instanceId))
    return
  }
  selectedIds.value = [...new Set([...selectedIds.value, ...visibleIds])]
}

function submit() {
  if (selectedIds.value.length) emit('confirm', selectedIds.value)
}
</script>

<template>
  <div class="modal-backdrop">
    <section
      class="modal rollout-selector-modal"
      role="dialog"
      aria-modal="true"
      aria-labelledby="rollout-selector-title"
    >
      <header class="modal-header">
        <div class="modal-title">
          <span><Server :size="20" /></span>
          <div>
            <h2 id="rollout-selector-title">
              {{ mode === 'publish' ? '选择首批灰度实例' : '添加灰度批次' }}
            </h2>
            <p>Agent {{ release.version }}</p>
          </div>
        </div>
        <button class="icon-button subtle" type="button" title="关闭" aria-label="关闭实例选择" @click="$emit('close')">
          <X :size="17" />
        </button>
      </header>

      <div class="rollout-selector-filters">
        <label class="rollout-search">
          <Search :size="15" />
          <input v-model="search" type="search" placeholder="搜索名称、主机名或实例 ID" autocomplete="off" />
        </label>
        <label>
          <span>状态</span>
          <select v-model="statusFilter">
            <option value="all">全部状态</option>
            <option value="online">在线</option>
            <option value="offline">离线</option>
          </select>
        </label>
        <label>
          <span>平台</span>
          <select v-model="platformFilter">
            <option value="all">全部平台</option>
            <option v-for="platform in platforms" :key="platform" :value="platform">{{ platform }}</option>
          </select>
        </label>
      </div>

      <div class="rollout-selection-summary">
        <label>
          <input
            type="checkbox"
            :checked="allVisibleSelected"
            :indeterminate="someVisibleSelected"
            :disabled="!selectableCandidates.length"
            @change="toggleVisible"
          />
          <span>选择当前结果</span>
        </label>
        <span>{{ selectedIds.length }} 已选 · {{ filteredCandidates.length }} 个结果</span>
      </div>

      <div class="rollout-candidate-list" aria-live="polite">
        <div v-if="loading" class="rollout-selector-empty">
          <LoaderCircle class="spin" :size="20" />正在加载实例
        </div>
        <div v-else-if="!filteredCandidates.length" class="rollout-selector-empty">
          没有符合筛选条件的实例
        </div>
        <template v-else>
          <button
            v-for="candidate in filteredCandidates"
            :key="candidate.instance_id"
            class="rollout-candidate-row"
            :class="{
              selected: selectedIds.includes(candidate.instance_id),
              unavailable: !isSelectable(candidate),
            }"
            type="button"
            :disabled="!isSelectable(candidate)"
            @click="toggleCandidate(candidate)"
          >
            <span class="rollout-candidate-check" aria-hidden="true">
              <Check v-if="selectedIds.includes(candidate.instance_id)" :size="13" />
            </span>
            <span :class="['rollout-candidate-online', { online: candidate.online }]">
              <Wifi v-if="candidate.online" :size="15" />
              <WifiOff v-else :size="15" />
            </span>
            <span class="rollout-candidate-identity">
              <strong>{{ candidate.name || candidate.hostname }}</strong>
              <small>{{ candidate.hostname }} · {{ candidate.instance_id.slice(0, 12) }}</small>
            </span>
            <span class="rollout-candidate-platform">
              <strong>{{ candidate.os }} / {{ candidate.native_arch || '未知架构' }}</strong>
              <small>{{ candidate.package_type || '未上报格式' }} · v{{ candidate.agent_version || '未知' }}</small>
              <small :class="['rollout-candidate-rollback', { unsupported: !candidate.rollback_supported }]">
                {{ candidate.rollback_supported
                  ? (candidate.rollback_version ? `本地基线 v${candidate.rollback_version}` : '支持主动回滚')
                  : '不支持主动回滚' }}
              </small>
            </span>
            <span v-if="candidate.selected" class="rollout-candidate-reason selected-reason">已在灰度批次</span>
            <span v-else-if="candidate.reason" class="rollout-candidate-reason">
              <ShieldAlert :size="13" />{{ candidate.reason }}
            </span>
            <span v-else class="rollout-candidate-reason ready">
              {{ candidate.online ? '可立即下发' : '上线后下发' }}
            </span>
          </button>
        </template>
      </div>

      <form class="modal-actions" @submit.prevent="submit">
        <button class="text-button" type="button" @click="$emit('close')">取消</button>
        <button class="primary-button" type="submit" :disabled="loading || !selectedIds.length">
          {{ mode === 'publish' ? `发布到 ${selectedIds.length} 个实例` : `添加 ${selectedIds.length} 个实例` }}
        </button>
      </form>
    </section>
  </div>
</template>
