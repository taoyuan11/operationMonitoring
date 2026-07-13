<script setup lang="ts">
import { KeyRound, LoaderCircle, LockKeyhole, LogIn, RotateCcw, ShieldCheck, User, X } from 'lucide-vue-next'
import QrcodeVue from 'qrcode.vue'
import type { AuthEnrollment, AuthMode } from '../types/domain'

const props = defineProps<{
  loading: boolean
  errorMessage: string
  mode: AuthMode
  enrollment: AuthEnrollment | null
  form: {
    username: string
    password: string
    code: string
  }
}>()

defineEmits<{
  close: []
  login: []
  restart: []
}>()

function canSubmit() {
  if (props.enrollment) return /^\d{6}$/.test(props.form.code.trim())
  if (props.mode === 'bootstrap') return Boolean(props.form.username.trim() && props.form.password)
  return Boolean(props.form.username.trim() && /^\d{6}$/.test(props.form.code.trim()))
}
</script>

<template>
  <div class="modal-backdrop login-backdrop" @click.self="$emit('close')" @keydown.esc="$emit('close')">
    <form class="modal login-modal" role="dialog" aria-modal="true" aria-labelledby="login-title" @submit.prevent="$emit('login')">
      <button class="icon-button subtle modal-close" type="button" title="关闭" @click="$emit('close')">
        <X :size="17" />
      </button>
      <div class="login-icon"><ShieldCheck :size="25" /></div>
      <div class="login-copy centered">
        <span class="section-kicker">Authenticator access</span>
        <h2 id="login-title">{{ mode === 'bootstrap' ? '初始化管理员认证' : '管理员登录' }}</h2>
        <p v-if="enrollment">使用手机 Authenticator 扫描二维码，然后输入当前生成的 6 位代码。</p>
        <p v-else-if="mode === 'bootstrap'">首次启动需使用默认密码创建首位管理员并绑定认证设备。</p>
        <p v-else>输入用户名和手机 Authenticator 中显示的 6 位代码。</p>
      </div>
      <Transition name="notice">
        <p v-if="errorMessage" class="login-error">{{ errorMessage }}</p>
      </Transition>

      <template v-if="enrollment">
        <div class="auth-qr-card">
          <QrcodeVue :value="enrollment.otpauth_uri" :size="190" level="M" :margin="2" />
          <strong>{{ enrollment.username }}</strong>
          <small>{{ enrollment.device_name }} · 二维码将在 10 分钟后失效</small>
        </div>
        <label class="password-field">
          <span>Authenticator 代码</span>
          <div class="input-shell">
            <KeyRound :size="16" />
            <input
              v-model="form.code"
              inputmode="numeric"
              autocomplete="one-time-code"
              maxlength="6"
              autofocus
              placeholder="000000"
            />
          </div>
        </label>
      </template>

      <template v-else>
        <label class="password-field">
          <span>用户名</span>
          <div class="input-shell">
            <User :size="16" />
            <input
              v-model="form.username"
              autocomplete="username"
              autofocus
              placeholder="例如：admin"
            />
          </div>
        </label>
        <label v-if="mode === 'bootstrap'" class="password-field">
          <span>默认管理密码</span>
          <div class="input-shell">
            <LockKeyhole :size="16" />
            <input
              v-model="form.password"
              type="password"
              autocomplete="current-password"
              placeholder="请输入默认密码"
            />
          </div>
        </label>
        <label v-else class="password-field">
          <span>Authenticator 代码</span>
          <div class="input-shell">
            <KeyRound :size="16" />
            <input
              v-model="form.code"
              inputmode="numeric"
              autocomplete="one-time-code"
              maxlength="6"
              placeholder="000000"
            />
          </div>
        </label>
      </template>

      <button class="primary-button login-button" type="submit" :disabled="loading || !canSubmit()">
        <LoaderCircle v-if="loading" class="spin" :size="17" />
        <LogIn v-else :size="17" />
        {{ loading ? '正在验证' : enrollment ? '确认并进入管理台' : mode === 'bootstrap' ? '生成绑定二维码' : '登录管理台' }}
      </button>
      <button v-if="enrollment && mode === 'bootstrap'" class="text-button auth-restart" type="button" @click="$emit('restart')">
        <RotateCcw :size="14" />重新输入用户名和密码
      </button>
      <small class="login-hint"><ShieldCheck :size="13" />初始化完成后，默认密码将永久失效</small>
    </form>
  </div>
</template>
