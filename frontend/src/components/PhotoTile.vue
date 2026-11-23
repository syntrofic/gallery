<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { Image as ImageIcon } from 'lucide-vue-next'
import { mediaPath, type Photo } from '../api'

const props = defineProps<{ photo?: Photo | null; album?: string; label: string; eager?: boolean }>()
const failed = ref(false)
const variant = computed(() => props.photo?.variants?.find(item => item.spec_id === 'thumb') || props.photo?.variants?.[0])
const source = computed(() => props.photo && variant.value ? mediaPath(props.album, props.photo.id, variant.value.id) : '')
watch(source, () => { failed.value = false })
</script>

<template>
  <div class="photo-tile">
    <img v-if="source && !failed" :src="source" alt="" :loading="eager ? 'eager' : 'lazy'" decoding="async" @error="failed = true" />
    <span v-else class="photo-placeholder" :aria-label="label"><ImageIcon :size="26" :stroke-width="1.25" aria-hidden="true" /></span>
  </div>
</template>
