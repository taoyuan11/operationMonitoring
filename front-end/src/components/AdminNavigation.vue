<script setup lang="ts">
import { ClipboardList, Home, ListChecks, PackageCheck, Settings, Terminal, Users } from 'lucide-vue-next'
import type { AppPage } from '../types/domain'

defineProps<{
  currentPage: AppPage
  pendingCount: number
}>()

defineEmits<{
  navigate: [page: AppPage]
}>()

const items: Array<{ page: AppPage; label: string; icon: typeof Home }> = [
  { page: 'home', label: '实例概览', icon: Home },
  { page: 'pending', label: '接入审核', icon: ListChecks },
  { page: 'commands', label: '快捷命令', icon: Terminal },
  { page: 'updates', label: '程序更新', icon: PackageCheck },
  { page: 'users', label: '用户管理', icon: Users },
  { page: 'logs', label: '操作日志', icon: ClipboardList },
  { page: 'settings', label: '系统设置', icon: Settings },
]
</script>

<template>
  <nav class="admin-navigation" aria-label="管理员导航">
    <button
      v-for="item in items"
      :key="item.page"
      :class="{ active: currentPage === item.page }"
      type="button"
      :aria-label="item.label"
      :aria-current="currentPage === item.page ? 'page' : undefined"
      @click="$emit('navigate', item.page)"
    >
      <component :is="item.icon" :size="16" />
      <span>{{ item.label }}</span>
      <em v-if="item.page === 'pending' && pendingCount">{{ pendingCount }}</em>
    </button>
  </nav>
</template>
