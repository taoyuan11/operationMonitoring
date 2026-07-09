<script setup lang="ts">
import { Grid3X3, Pause, Pencil, Play, Rows3, Server, Terminal, Trash2 } from 'lucide-vue-next'
import type { CommandRecord, Instance, ViewMode } from '../types/domain'
import { formatBytes, formatDuration, formatPercent, formatTime, metricPercent } from '../utils/format'

defineProps<{
  instances: Instance[]
  commands: CommandRecord[]
  isAdmin: boolean
  viewMode: ViewMode
}>()

defineEmits<{
  'update:viewMode': [value: ViewMode]
  edit: [instance: Instance]
  terminal: [instance: Instance]
  disable: [instance: Instance]
  delete: [instance: Instance]
  runCommand: [instance: Instance, command: CommandRecord]
}>()
</script>

<template>
  <div class="content-pane">
    <div class="section-head">
      <div>
        <h2>实例看板</h2>
        <p>公开只读数据实时刷新，管理员登录后可执行管理操作。</p>
      </div>
      <div class="segmented">
        <button :class="{ active: viewMode === 'grid' }" type="button" @click="$emit('update:viewMode', 'grid')">
          <Grid3X3 :size="16" />
        </button>
        <button :class="{ active: viewMode === 'rows' }" type="button" @click="$emit('update:viewMode', 'rows')">
          <Rows3 :size="16" />
        </button>
      </div>
    </div>

    <div v-if="instances.length === 0" class="empty-state">
      <Server :size="32" />
      <strong>暂无已批准实例</strong>
      <span>启动实例端后，管理员在待审批列表批准即可显示。</span>
    </div>

    <div v-else :class="['instance-list', viewMode]">
      <article
        v-for="instance in instances"
        :key="instance.id"
        :class="['instance-card', { offline: !instance.online }]"
      >
        <div class="instance-main">
          <div>
            <div class="title-line">
              <span :class="['status-dot', { online: instance.online }]"></span>
              <h3>{{ instance.name || instance.hostname }}</h3>
            </div>
            <p>{{ instance.region || '未设置地区' }} · {{ instance.os }}/{{ instance.arch }}</p>
            <small>{{ instance.remark || instance.hostname }}</small>
          </div>
          <div class="instance-actions" v-if="isAdmin">
            <button class="icon-button" type="button" title="编辑" @click="$emit('edit', instance)">
              <Pencil :size="16" />
            </button>
            <button class="icon-button" type="button" title="Web 终端" @click="$emit('terminal', instance)">
              <Terminal :size="16" />
            </button>
            <button class="icon-button danger" type="button" title="停用" @click="$emit('disable', instance)">
              <Pause :size="16" />
            </button>
            <button class="icon-button danger" type="button" title="删除" @click="$emit('delete', instance)">
              <Trash2 :size="16" />
            </button>
          </div>
        </div>

        <div class="metrics">
          <div class="metric">
            <span>CPU</span>
            <strong>{{ formatPercent(instance.metrics?.cpu_percent) }}</strong>
            <i><b :style="{ width: `${instance.metrics?.cpu_percent || 0}%` }"></b></i>
          </div>
          <div class="metric">
            <span>内存</span>
            <strong>{{ formatBytes(instance.metrics?.memory_used) }} / {{ formatBytes(instance.metrics?.memory_total) }}</strong>
            <i>
              <b
                :style="{
                  width: `${metricPercent(instance.metrics?.memory_used || 0, instance.metrics?.memory_total || 0)}%`,
                }"
              ></b>
            </i>
          </div>
          <div class="metric">
            <span>硬盘</span>
            <strong>{{ formatBytes(instance.metrics?.disk_used) }} / {{ formatBytes(instance.metrics?.disk_total) }}</strong>
            <i>
              <b
                :style="{
                  width: `${metricPercent(instance.metrics?.disk_used || 0, instance.metrics?.disk_total || 0)}%`,
                }"
              ></b>
            </i>
          </div>
          <div class="metric">
            <span>GPU</span>
            <strong>{{ formatPercent(instance.metrics?.gpu_percent) }}</strong>
            <i><b :style="{ width: `${instance.metrics?.gpu_percent || 0}%` }"></b></i>
          </div>
        </div>

        <div class="meta-grid">
          <span>在线时长 <strong>{{ formatDuration(instance.metrics?.uptime_seconds) }}</strong></span>
          <span>网络接收 <strong>{{ formatBytes(instance.metrics?.network_rx) }}</strong></span>
          <span>网络发送 <strong>{{ formatBytes(instance.metrics?.network_tx) }}</strong></span>
          <span>最后上报 <strong>{{ formatTime(instance.last_seen) }}</strong></span>
        </div>

        <div v-if="isAdmin && commands.length" class="quick-row">
          <button
            v-for="command in commands"
            :key="command.id"
            class="text-button compact"
            type="button"
            @click="$emit('runCommand', instance, command)"
          >
            <Play :size="15" />{{ command.name }}
          </button>
        </div>
      </article>
    </div>
  </div>
</template>
