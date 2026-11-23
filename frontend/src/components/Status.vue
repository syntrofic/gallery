<script setup lang="ts">
import { SBadge } from '@syntrofic/ui'
import { statusText } from '../api'
defineProps<{ state: string }>()
</script>

<template>
  <SBadge :tone="['failed', 'unavailable'].includes(state) ? 'danger' : ['ready', 'succeeded', 'committed', 'verified'].includes(state) ? 'success' : 'neutral'">
    {{ statusText(state) }}
  </SBadge>
</template>
