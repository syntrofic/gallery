<script setup lang="ts">
import { SAlert } from '@syntrofic/ui'
defineProps<{ error?: string; message?: string }>()
</script>

<template>
  <SAlert v-if="error" tone="danger" announcement="assertive">{{ error }}</SAlert>
  <SAlert v-else-if="message" tone="success" announcement="polite">{{ message }}</SAlert>
</template>
