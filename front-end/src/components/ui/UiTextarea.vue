<script setup lang="ts">
const [model, modifiers] = defineModel<string>({ set: (value) => modifiers.trim ? value.trim() : value })
</script>

<template>
  <textarea v-model="model" class="ui-textarea" />
</template>
