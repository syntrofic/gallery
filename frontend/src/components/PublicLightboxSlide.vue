<script setup lang="ts">
import { computed, onBeforeUnmount, ref, watch } from 'vue'
import { SButton, SSpinner } from '@syntrofic/ui'
import { ImageOff } from 'lucide-vue-next'
import { errorText, mediaPath, photoPath, query, request, visibleCaption, type Photo } from '../api'
import { fittedVariants, mediaSrcset, photoAspectRatio } from '../galleryMedia'
import type { GalleryPhotoRef } from '../galleryPhoto'

const props = defineProps<{
  selection: GalleryPhotoRef
  seed?: Photo
  active: boolean
  preload: boolean
  sort?: string
  offset: number
  viewport: { width: number; height: number }
  zoom: { scale: number; x: number; y: number }
  original: boolean
  resolutionScale: number
}>()
const emit = defineEmits<{
  detail: [photo: Photo | undefined]
  preview: [state: { zoomable: boolean; settled: boolean; url: string; width: number; height: number }]
  originalError: []
}>()

const detail = ref<Photo>()
const error = ref('')
const loading = ref(false)
const requested = ref(false)
const failed = ref(new Set<string>())
const previewUrl = ref('')
const originalReady = ref(false)
let loader: AbortController | undefined

const photo = computed(() => detail.value ?? props.seed)
const aspect = computed(() => photo.value ? photoAspectRatio(photo.value) : 1)
const width = computed(() => {
  if (!photo.value || !props.viewport.width || !props.viewport.height) return 0
  const originalWidth = Math.max(photo.value.width, photo.value.height) * Math.min(1, aspect.value)
  return Math.min(props.viewport.width, props.viewport.height * aspect.value, originalWidth)
})
const height = computed(() => width.value / aspect.value)
const variants = computed(() => {
  const available = (photo.value?.variants ?? []).filter(variant => !failed.value.has(variant.id))
  const fitted = fittedVariants(available, aspect.value)
  const details = fitted.filter(variant => variant.spec_id !== 'thumb')
  return { main: details.length ? details : fitted.length ? fitted : available, thumb: fitted.find(variant => variant.spec_id === 'thumb') }
})
const source = computed(() => {
  const id = (variants.value.main.find(variant => variant.spec_id === 'mobile') ?? variants.value.main[0])?.id
  return id && requested.value && width.value ? mediaPath(props.selection.albumId, props.selection.imageId, id) : ''
})
const thumbnail = computed(() => {
  const id = variants.value.thumb?.id
  return id ? mediaPath(props.selection.albumId, props.selection.imageId, id) : ''
})
const hasPoster = computed(() => !!source.value && !!thumbnail.value && thumbnail.value !== source.value)
const srcset = computed(() => mediaSrcset(variants.value.main, props.selection.albumId, props.selection.imageId))
const originalUrl = computed(() => '/api/v1' + photoPath(props.selection.albumId, props.selection.imageId) + '/original-view')
const caption = computed(() => visibleCaption(photo.value))
const transform = computed(() => props.active
  ? `translate3d(${props.zoom.x}px, ${props.zoom.y}px, 0) scale(${props.zoom.scale})`
  : undefined)

async function load() {
  loader?.abort()
  const controller = new AbortController()
  loader = controller
  loading.value = true
  error.value = ''
  try {
    const result = await request<Photo>(photoPath(props.selection.albumId, props.selection.imageId) + query({ sort: props.sort }),
      { signal: controller.signal })
    if (!controller.signal.aborted) detail.value = result
  } catch (failure) {
    if (!controller.signal.aborted) error.value = errorText(failure)
  } finally {
    if (!controller.signal.aborted) loading.value = false
  }
}
function retry() {
  failed.value.clear()
  void load()
}
async function reveal(event: Event, original = false) {
  const element = event.currentTarget as HTMLImageElement
  const url = element.currentSrc
  try { await element.decode() } catch { /* A browser can draw a valid loaded image even when decode rejects. */ }
  if (!element.isConnected || !element.naturalWidth || url !== element.currentSrc) return
  if (original) originalReady.value = true
  else previewUrl.value = url
}
function fail(event: Event) {
  const element = event.currentTarget as HTMLImageElement
  const id = (element.currentSrc || element.src).match(/\/variants\/([^/]+)\/body$/)?.[1]
  if (id) failed.value.add(decodeURIComponent(id))
}

watch(() => props.sort, () => {
  loader?.abort()
  loader = undefined
  detail.value = undefined
  loading.value = false
  if (props.active || requested.value && !props.seed) void load()
})
watch([() => props.active, () => props.preload, () => props.seed], ([active, preload, seed]) => {
  if (active || preload) requested.value = true
  if ((active || preload && !seed) && !loader) void load()
}, { immediate: true })
watch(source, () => { previewUrl.value = '' })
watch(() => props.original, () => { originalReady.value = false })
watch([() => props.active, detail], ([active, loaded]) => {
  if (active) emit('detail', loaded)
}, { immediate: true })
watch([() => props.active, previewUrl, originalReady, source, loading, width, height],
  ([active, url, loadedOriginal, src, pending, w, h]) => {
    if (!active) return
    const settled = !!url || !pending && (!photo.value || !!w && !src)
    emit('preview', { zoomable: !!url || loadedOriginal, settled, url, width: w, height: h })
  }, { immediate: true })
onBeforeUnmount(() => loader?.abort())
</script>

<template>
  <div class="lightbox-slide" :style="{ left: `${offset * 100}%` }" :aria-hidden="active ? undefined : true">
    <div v-if="source || active && original && width" class="lightbox-picture" :style="{ width: `${width}px`, height: `${height}px`, transform }">
      <img v-if="hasPoster" class="lightbox-thumbnail" :class="{ 'is-covered': !!previewUrl }"
        :src="thumbnail" alt="" aria-hidden="true" decoding="async" :fetchpriority="active ? 'auto' : 'low'" draggable="false" />
      <img v-if="source" :key="source" :src="source" :srcset="srcset" :sizes="`${Math.ceil(width * (active ? resolutionScale : 1))}px`"
        :alt="active ? caption?.text || '照片' : ''" :class="{ 'is-visible': !!previewUrl, 'is-ready': active && !!previewUrl }"
        :fetchpriority="active ? 'high' : 'low'" decoding="async" draggable="false" @load="reveal($event)" @error="fail" />
      <img v-if="active && original" :src="originalUrl" :alt="source ? '' : caption?.text || '照片'" :aria-hidden="source ? true : undefined"
        class="lightbox-original" :class="{ 'is-visible': originalReady }" decoding="async" draggable="false"
        fetchpriority="high" @load="reveal($event, true)" @error="emit('originalError')" />
    </div>
    <div v-if="active && !originalReady && !previewUrl && (original || loading || source) && !hasPoster"
      class="lightbox-status" role="status"><SSpinner /><span class="sr-only">加载照片</span></div>
    <div v-else-if="active && !loading && !source && !original && viewport.width" class="lightbox-status" role="status">
      <ImageOff :size="27" aria-hidden="true" /><span>{{ error || '预览暂不可用' }}</span>
      <SButton size="sm" variant="secondary" @click="retry">重试</SButton>
    </div>
  </div>
</template>
