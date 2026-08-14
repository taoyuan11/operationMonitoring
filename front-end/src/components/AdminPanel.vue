<script setup lang="ts">
import { computed } from 'vue'
import {
  Check,
  Image,
  ListChecks,
  LoaderCircle,
  Monitor,
  Moon,
  Palette,
  Settings,
  Sun,
  Terminal,
  Trash2,
  X,
} from '@lucide/vue'
import type {
  AdminTab,
  PendingInstance,
  ResolvedTheme,
  ThemeMode,
} from '../types/domain'
import { formatTime } from '../utils/format'

const props = defineProps<{
  adminTab: AdminTab
  pendingInstances: PendingInstance[]
  settingsForm: {
    retention_days: number
    audit_retention_days: number
    alert_retention_days: number
    background_image_url: string | null
    theme_mode: ThemeMode
    accent_color: string
  }
  resolvedTheme: ResolvedTheme
  appearanceMessage: string
  backgroundFileName: string
  backgroundOperation: 'uploading' | 'removing' | null
  backgroundMessage: string
}>()

const themeOptions = [
  { value: 'auto' as const, label: '跟随系统', description: '自动使用设备的明暗外观', icon: Monitor },
  { value: 'light' as const, label: '明亮', description: '始终使用浅色界面', icon: Sun },
  { value: 'dark' as const, label: '暗黑', description: '始终使用深色界面', icon: Moon },
]

const accentPresets = [
  { label: '翠绿', value: '#3bbf9b' },
  { label: '海蓝', value: '#3b82f6' },
  { label: '青蓝', value: '#159eaa' },
  { label: '紫罗兰', value: '#8b5cf6' },
  { label: '琥珀', value: '#d08a24' },
  { label: '玫红', value: '#d95f80' },
]

const previewTheme = computed(() =>
  props.settingsForm.theme_mode === 'auto' ? props.resolvedTheme : props.settingsForm.theme_mode,
)

const emit = defineEmits<{
  approve: [id: string]
  reject: [id: string]
  saveSettings: []
  saveAppearance: []
  appearanceChanged: []
  selectBackgroundImage: [event: Event]
  clearBackgroundImage: []
}>()
</script>

<template>
  <section class="management-page">
    <template v-if="adminTab === 'pending'">
      <header class="page-header">
        <div class="page-heading-icon"><ListChecks :size="22" /></div>
        <div>
          <span class="section-kicker">Access review</span>
          <h2>接入审核</h2>
          <p>审核新节点的接入请求，仅批准可信设备。</p>
        </div>
        <span class="page-count">{{ pendingInstances.length }} 个待处理</span>
      </header>

      <div class="admin-content-card wide-card admin-list-card">
        <div class="card-heading">
          <div><h3>待审批节点</h3><p>节点在获得批准之前不会出现在公开实例列表中。</p></div>
        </div>
        <Transition name="content" mode="out-in">
          <div v-if="pendingInstances.length === 0" key="empty" class="page-empty">
            <span><Check :size="24" /></span>
            <strong>所有申请均已处理</strong>
            <p>目前没有等待审核的新节点。</p>
          </div>
          <TransitionGroup v-else key="approvals" name="row" tag="div" class="approval-list">
            <article v-for="item in pendingInstances" :key="item.id" class="approval-row">
              <span class="list-icon"><Terminal :size="17" /></span>
              <div class="approval-identity">
                <strong>{{ item.hostname }}</strong>
                <span>{{ item.os }}/{{ item.arch }} · Agent {{ item.agent_version }}</span>
              </div>
              <div class="approval-time">
                <span>最后请求</span>
                <strong>{{ formatTime(item.last_seen) }}</strong>
              </div>
              <div class="row-actions">
                <button class="approve-button" type="button" @click="$emit('approve', item.id)">
                  <Check :size="15" />批准
                </button>
                <button class="reject-button" type="button" @click="$emit('reject', item.id)">
                  <X :size="15" />拒绝
                </button>
              </div>
            </article>
          </TransitionGroup>
        </Transition>
      </div>
    </template>

    <template v-if="adminTab === 'settings'">
      <header class="page-header">
        <div class="page-heading-icon amber"><Settings :size="22" /></div>
        <div>
          <span class="section-kicker">System preferences</span>
          <h2>系统设置</h2>
          <p>管理数据生命周期与监控页面的视觉外观。</p>
        </div>
      </header>

      <div class="admin-page-grid settings-layout">
        <div class="admin-content-card appearance-settings-card">
          <div class="card-heading">
            <div><h3>外观主题</h3><p>设置所有访问者看到的界面明暗模式与主要强调色。</p></div>
            <span class="appearance-current">当前显示：{{ resolvedTheme === 'light' ? '明亮' : '暗黑' }}</span>
          </div>

          <div class="appearance-settings-body">
            <div class="appearance-controls">
              <fieldset class="theme-mode-fieldset">
                <legend>主题模式</legend>
                <div class="theme-mode-options">
                  <button
                    v-for="option in themeOptions"
                    :key="option.value"
                    :class="['theme-mode-option', { active: settingsForm.theme_mode === option.value }]"
                    type="button"
                    :aria-pressed="settingsForm.theme_mode === option.value"
                    @click="settingsForm.theme_mode = option.value; $emit('appearanceChanged')"
                  >
                    <component :is="option.icon" :size="18" />
                    <span><strong>{{ option.label }}</strong><small>{{ option.description }}</small></span>
                    <i aria-hidden="true"></i>
                  </button>
                </div>
              </fieldset>

              <fieldset class="accent-fieldset">
                <legend>主题色</legend>
                <div class="accent-presets">
                  <button
                    v-for="preset in accentPresets"
                    :key="preset.value"
                    :class="['accent-swatch', { active: settingsForm.accent_color === preset.value }]"
                    :style="{ '--swatch-color': preset.value }"
                    type="button"
                    :title="preset.label"
                    :aria-label="`使用${preset.label}主题色`"
                    :aria-pressed="settingsForm.accent_color === preset.value"
                    @click="settingsForm.accent_color = preset.value; $emit('appearanceChanged')"
                  ><Check v-if="settingsForm.accent_color === preset.value" :size="14" /></button>
                  <label class="custom-color-picker">
                    <Palette :size="16" />
                    <span>自定义</span>
                    <input v-model="settingsForm.accent_color" type="color" aria-label="选择自定义主题色" @input="$emit('appearanceChanged')" />
                    <code>{{ settingsForm.accent_color }}</code>
                  </label>
                </div>
              </fieldset>

              <div class="appearance-save-row">
                <button class="primary-button" type="button" @click="$emit('saveAppearance')">
                  <Palette :size="16" />保存外观
                </button>
                <small :class="{ success: appearanceMessage }">{{ appearanceMessage || '保存后将作为全站默认外观' }}</small>
              </div>
            </div>

            <div
              :class="['appearance-preview', `preview-${previewTheme}`]"
              :style="{ '--preview-accent': settingsForm.accent_color }"
            >
              <span class="preview-label">实时预览</span>
              <div class="preview-window">
                <header><i></i><i></i><i></i></header>
                <section>
                  <div class="preview-sidebar"><span></span><span class="active"></span><span></span></div>
                  <div class="preview-content">
                    <div class="preview-summary"><i></i><i></i><i></i></div>
                    <div class="preview-card"><strong>运行监控</strong><span></span><button type="button">主要操作</button></div>
                  </div>
                </section>
              </div>
              <small>{{ previewTheme === 'light' ? '明亮主题预览' : '暗黑主题预览' }}</small>
            </div>
          </div>
        </div>

        <div class="admin-content-card background-card">
          <div class="card-heading"><div><h3>页面背景</h3><p>自定义监控页面背景，系统会自动添加深色遮罩。</p></div></div>
          <div :class="['large-background-preview', { empty: !settingsForm.background_image_url }]">
            <Transition name="fade" mode="out-in">
              <img
                v-if="settingsForm.background_image_url"
                key="image"
                :src="settingsForm.background_image_url"
                alt="当前背景"
              />
              <div v-else key="empty"><Image :size="28" /><span>当前使用默认渐变背景</span></div>
            </Transition>
          </div>
          <div class="background-toolbar">
            <label :class="['file-button', { disabled: backgroundOperation }]">
              <LoaderCircle v-if="backgroundOperation === 'uploading'" class="spin" :size="15" />
              <Image v-else :size="15" />
              {{ backgroundOperation === 'uploading' ? '正在上传' : '选择图片' }}
              <input
                type="file"
                accept="image/png,image/jpeg,image/webp"
                :disabled="Boolean(backgroundOperation)"
                @change="$emit('selectBackgroundImage', $event)"
              />
            </label>
            <Transition name="fade-scale">
              <button
                v-if="settingsForm.background_image_url"
                class="text-button danger"
                type="button"
                :disabled="Boolean(backgroundOperation)"
                @click="$emit('clearBackgroundImage')"
              >
                <LoaderCircle v-if="backgroundOperation === 'removing'" class="spin" :size="14" />
                <Trash2 v-else :size="14" />
                {{ backgroundOperation === 'removing' ? '正在移除' : '移除背景' }}
              </button>
            </Transition>
            <small :class="{ success: backgroundMessage }">{{ backgroundMessage || backgroundFileName || '支持 PNG、JPEG、WebP，最大 5MB' }}</small>
          </div>
        </div>

        <div class="admin-content-card retention-card">
          <div class="card-heading"><div><h3>数据保留</h3><p>分别设置历史指标、审计事件和已恢复告警的自动清理期限。</p></div></div>
          <form class="stack-form page-form" @submit.prevent="$emit('saveSettings')">
            <label>
              <span>指标保留天数</span>
              <input v-model.number="settingsForm.retention_days" min="1" max="365" type="number" />
            </label>
            <small class="form-help">可设置 1 至 365 天，不影响节点基础信息。</small>
            <label>
              <span>审计保留天数</span>
              <input v-model.number="settingsForm.audit_retention_days" min="1" max="3650" type="number" />
            </label>
            <small class="form-help">可设置 1 至 3650 天，运行中的会话不会被清理。</small>
            <label>
              <span>告警保留天数</span>
              <input v-model.number="settingsForm.alert_retention_days" min="1" max="3650" type="number" />
            </label>
            <small class="form-help">仅清理过期的已恢复事件及投递记录，活动事件不会被清理。</small>
            <button class="primary-button" type="submit"><Settings :size="16" />保存设置</button>
          </form>
        </div>
      </div>
    </template>

  </section>
</template>
