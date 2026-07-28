<script setup lang="ts">
import type { DockerComposeValidation } from '../types/docker'

defineProps<{
  validation: DockerComposeValidation
}>()
</script>

<template>
  <section :class="['docker-compose-validation', { invalid: !validation.valid }]">
    <div class="docker-compose-validation-head">
      <strong>{{ validation.valid ? '配置校验通过' : '配置校验失败' }}</strong>
      <span v-if="validation.project_name">{{ validation.project_name }}</span>
    </div>

    <div class="docker-compose-counts" aria-label="Compose 配置资源计数">
      <div><strong>{{ validation.config_summary.service_count }}</strong><span>服务</span></div>
      <div><strong>{{ validation.config_summary.network_count }}</strong><span>网络</span></div>
      <div><strong>{{ validation.config_summary.volume_count }}</strong><span>存储卷</span></div>
      <div><strong>{{ validation.config_summary.config_count }}</strong><span>配置项</span></div>
      <div><strong>{{ validation.config_summary.secret_count }}</strong><span>密钥</span></div>
    </div>

    <div v-if="validation.service_summaries.length" class="docker-compose-services">
      <div
        v-for="service in validation.service_summaries"
        :key="service.name"
        class="docker-compose-service"
      >
        <div class="docker-compose-service-name">
          <strong>{{ service.name }}</strong>
          <code>{{ service.image || '未指定镜像' }}</code>
        </div>
        <dl>
          <div><dt>端口</dt><dd>{{ service.ports.join('、') || '—' }}</dd></div>
          <div><dt>挂载</dt><dd>{{ service.mounts.join('、') || '—' }}</dd></div>
          <div><dt>网络</dt><dd>{{ service.networks.join('、') || '—' }}</dd></div>
          <div><dt>Profiles</dt><dd>{{ service.profiles.join('、') || '—' }}</dd></div>
        </dl>
      </div>
    </div>

    <code v-if="validation.config_digest" class="docker-compose-digest">
      {{ validation.config_digest }}
    </code>
    <ul v-if="validation.warnings.length" class="docker-compose-warnings">
      <li v-for="warning in validation.warnings" :key="warning">{{ warning }}</li>
    </ul>
  </section>
</template>
