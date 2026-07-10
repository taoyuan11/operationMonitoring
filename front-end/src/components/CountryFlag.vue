<script setup lang="ts">
import { computed } from 'vue'
import { getCountryFlagUrl, getCountryOption } from '../data/countries'

const props = defineProps<{
  code?: string
  name?: string
}>()

const country = computed(() => getCountryOption(props.code))
const flagUrl = computed(() => getCountryFlagUrl(props.code))
</script>

<template>
  <img
    v-if="country && flagUrl"
    class="country-flag"
    :src="flagUrl"
    :alt="`${name || country.name}国旗`"
  />
</template>
