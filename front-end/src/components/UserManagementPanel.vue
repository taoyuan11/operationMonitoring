<script setup lang="ts">
import {
  Ban,
  Check,
  KeyRound,
  LoaderCircle,
  Plus,
  ShieldCheck,
  Smartphone,
  Trash2,
  UserPlus,
  Users,
  X,
} from 'lucide-vue-next'
import QrcodeVue from 'qrcode.vue'
import type {
  AdminUser,
  AuthEnrollment,
  AuthenticatorDevice,
  PendingAuthEnrollment,
  SessionUser,
} from '../types/domain'
import { formatTime } from '../utils/format'

const props = defineProps<{
  users: AdminUser[]
  enrollments: PendingAuthEnrollment[]
  activeEnrollment: AuthEnrollment | null
  currentUser: SessionUser | null
  loading: boolean
  form: {
    username: string
    current_code: string
    confirmation_code: string
  }
}>()

defineEmits<{
  createUser: []
  addDevice: [user: AdminUser]
  confirmEnrollment: []
  cancelEnrollment: [id: string]
  setEnabled: [user: AdminUser, enabled: boolean]
  deleteUser: [user: AdminUser]
  revokeDevice: [device: AuthenticatorDevice]
}>()

function hasStepUpCode() {
  return /^\d{6}$/.test(props.form.current_code.trim())
}
</script>

<template>
  <section class="management-page user-management-page">
    <header class="page-header">
      <div class="page-heading-icon cyan"><Users :size="22" /></div>
      <div>
        <span class="section-kicker">Administrator identities</span>
        <h2>用户与认证设备</h2>
        <p>所有用户均拥有管理员权限，每台 Authenticator 可单独撤销。</p>
      </div>
      <span class="page-count">{{ users.length }} 位管理员</span>
    </header>

    <div class="admin-page-grid user-management-layout">
      <div class="admin-content-card auth-create-card">
        <div class="card-heading"><div><h3>添加管理员</h3><p>新用户需现场扫码并输入首个验证码后才会生效。</p></div></div>
        <form class="stack-form page-form" @submit.prevent="$emit('createUser')">
          <label>
            <span>新用户名</span>
            <input v-model="form.username" required minlength="3" maxlength="32" placeholder="字母、数字、点、下划线或连字符" />
          </label>
          <label>
            <span>当前管理员验证码</span>
            <input
              v-model="form.current_code"
              required
              inputmode="numeric"
              autocomplete="one-time-code"
              maxlength="6"
              placeholder="执行敏感操作前重新验证"
            />
          </label>
          <small class="form-help">添加、停用、删除或撤销设备时均需重新输入你自己的 6 位代码。</small>
          <button class="primary-button" type="submit" :disabled="loading || !form.username.trim() || !hasStepUpCode()">
            <LoaderCircle v-if="loading" class="spin" :size="16" />
            <UserPlus v-else :size="16" />生成二维码
          </button>
        </form>
      </div>

      <div v-if="activeEnrollment" class="admin-content-card active-enrollment-card">
        <div class="card-heading"><div><h3>确认新认证设备</h3><p>二维码只在当前页面临时展示，不支持下载。</p></div></div>
        <div class="enrollment-qr">
          <QrcodeVue :value="activeEnrollment.otpauth_uri" :size="180" level="M" :margin="2" />
          <strong>{{ activeEnrollment.username }} · {{ activeEnrollment.device_name }}</strong>
          <small>请在 {{ formatTime(activeEnrollment.expires_at) }} 前完成确认</small>
        </div>
        <form class="stack-form page-form enrollment-confirm-form" @submit.prevent="$emit('confirmEnrollment')">
          <label>
            <span>新设备生成的验证码</span>
            <input
              v-model="form.confirmation_code"
              required
              inputmode="numeric"
              autocomplete="one-time-code"
              maxlength="6"
              placeholder="000000"
            />
          </label>
          <button class="primary-button" type="submit" :disabled="loading || !/^\d{6}$/.test(form.confirmation_code.trim())">
            <Check :size="16" />确认并激活
          </button>
          <button
            class="text-button danger"
            type="button"
            :disabled="loading || !hasStepUpCode()"
            @click="$emit('cancelEnrollment', activeEnrollment.id)"
          >
            <X :size="14" />取消注册
          </button>
        </form>
      </div>

      <div class="admin-content-card wide-card administrator-list-card">
        <div class="card-heading"><div><h3>管理员账户</h3><p>停用用户会立即注销其全部会话，审计历史仍保留用户名。</p></div></div>
        <div class="administrator-list">
          <article v-for="user in users" :key="user.id" class="administrator-row">
            <div class="administrator-head">
              <span class="list-icon"><ShieldCheck :size="17" /></span>
              <div>
                <strong>{{ user.username }}</strong>
                <small>{{ user.id === currentUser?.id ? '当前用户 · ' : '' }}创建于 {{ formatTime(user.created_at) }}</small>
              </div>
              <span :class="['user-status', { disabled: !user.enabled }]">{{ user.enabled ? '已启用' : '已停用' }}</span>
              <div class="row-actions">
                <button
                  class="text-button"
                  type="button"
                  :disabled="loading || !hasStepUpCode()"
                  @click="$emit('addDevice', user)"
                ><Plus :size="14" />添加设备</button>
                <button
                  v-if="user.id !== currentUser?.id"
                  :class="['text-button', { danger: user.enabled }]"
                  type="button"
                  :disabled="loading || !hasStepUpCode()"
                  @click="$emit('setEnabled', user, !user.enabled)"
                ><Ban :size="14" />{{ user.enabled ? '停用' : '启用' }}</button>
                <button
                  v-if="user.id !== currentUser?.id"
                  class="icon-button danger"
                  type="button"
                  title="删除用户"
                  :disabled="loading || !hasStepUpCode()"
                  @click="$emit('deleteUser', user)"
                ><Trash2 :size="14" /></button>
              </div>
            </div>
            <div v-if="user.devices.length" class="auth-device-list">
              <div v-for="device in user.devices" :key="device.id" class="auth-device-row">
                <Smartphone :size="15" />
                <div><strong>{{ device.name }}</strong><small>最后使用：{{ device.last_used_at ? formatTime(device.last_used_at) : '尚未使用' }}</small></div>
                <button
                  class="icon-button danger"
                  type="button"
                  title="撤销设备"
                  :disabled="loading || !hasStepUpCode()"
                  @click="$emit('revokeDevice', device)"
                ><Trash2 :size="14" /></button>
              </div>
            </div>
            <p v-else class="compact-empty">该用户当前没有认证设备</p>
          </article>
        </div>
      </div>

      <div v-if="enrollments.length" class="admin-content-card wide-card pending-enrollment-card">
        <div class="card-heading"><div><h3>等待确认的注册</h3><p>页面刷新后二维码不会再次显示；可取消后重新生成。</p></div></div>
        <div class="auth-device-list">
          <div v-for="enrollment in enrollments" :key="enrollment.id" class="auth-device-row">
            <KeyRound :size="15" />
            <div><strong>{{ enrollment.username }} · {{ enrollment.device_name }}</strong><small>{{ formatTime(enrollment.expires_at) }} 过期</small></div>
            <button
              class="text-button danger"
              type="button"
              :disabled="loading || !hasStepUpCode()"
              @click="$emit('cancelEnrollment', enrollment.id)"
            ><X :size="14" />取消</button>
          </div>
        </div>
      </div>
    </div>
  </section>
</template>
