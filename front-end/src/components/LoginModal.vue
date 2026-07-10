<script setup lang="ts">
import { LoaderCircle, LockKeyhole, LogIn, ShieldCheck, X } from 'lucide-vue-next'

defineProps<{
  loading: boolean
  errorMessage: string
  form: {
    password: string
  }
}>()

defineEmits<{
  close: []
  login: []
}>()
</script>

<template>
  <div class="modal-backdrop login-backdrop" @click.self="$emit('close')" @keydown.esc="$emit('close')">
    <form class="modal login-modal" role="dialog" aria-modal="true" aria-labelledby="login-title" @submit.prevent="$emit('login')">
      <button class="icon-button subtle modal-close" type="button" title="关闭" @click="$emit('close')">
        <X :size="17" />
      </button>
      <div class="login-icon"><LockKeyhole :size="25" /></div>
      <div class="login-copy centered">
        <span class="section-kicker">Admin access</span>
        <h2 id="login-title">管理员登录</h2>
        <p>输入管理密码后即可进入控制台。</p>
      </div>
      <Transition name="notice">
        <p v-if="errorMessage" class="login-error">{{ errorMessage }}</p>
      </Transition>
      <label class="password-field">
        <span>管理密码</span>
        <div class="input-shell">
          <LockKeyhole :size="16" />
          <input
            v-model="form.password"
            type="password"
            autocomplete="current-password"
            autofocus
            placeholder="请输入密码"
          />
        </div>
      </label>
      <button class="primary-button login-button" type="submit" :disabled="loading || !form.password">
        <LoaderCircle v-if="loading" class="spin" :size="17" />
        <LogIn v-else :size="17" />
        {{ loading ? '正在验证' : '登录管理台' }}
      </button>
      <small class="login-hint"><ShieldCheck :size="13" />登录状态将通过安全会话保存</small>
    </form>
  </div>
</template>
