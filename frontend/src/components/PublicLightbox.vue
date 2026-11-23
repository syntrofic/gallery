<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter, type RouteLocationRaw } from 'vue-router'
import { SButton, SSpinner } from '@syntrofic/ui'
import { ArrowDownToLine, ChevronLeft, ChevronRight, ImageOff, Scan, X, ZoomIn, ZoomOut } from 'lucide-vue-next'
import { account, albumPath, errorText, mediaPath, photoPath, query, request, visibleCaption, type Album, type Photo } from '../api'
import { galleryPhotoLink, type GalleryPhotoRef } from '../galleryPhoto'

const props = defineProps<{
  selection: GalleryPhotoRef
  adjacent?: { previous?: GalleryPhotoRef; next?: GalleryPhotoRef }
  returnTo?: RouteLocationRaw
}>()
const route = useRoute()
const router = useRouter()
const dialog = ref<HTMLDialogElement>()
const stage = ref<HTMLElement>()
const image = ref<HTMLImageElement>()
const photo = ref<Photo>()
const caption = computed(() => visibleCaption(photo.value))
const album = ref<Album>()
const loading = ref(true)
const ready = ref(false)
const error = ref('')
const original = ref(false)
const fallback = ref(0)
const scale = ref(1)
const shift = ref({ x: 0, y: 0 })
let loader: AbortController | undefined
let closing = false

const base = computed(() => photoPath(props.selection.albumId, props.selection.imageId))
const variants = computed(() => {
  const image = photo.value
  const preferred = image?.selection?.[matchMedia('(max-width: 759px)').matches ? 'mobile' : 'desktop']?.variant_id
  return [...new Set([preferred, ...(image?.variants || []).map(variant => variant.id)].filter((id): id is string => Boolean(id)))]
})
const source = computed(() => {
  if (original.value) return '/api/v1' + base.value + '/original-view'
  const id = variants.value[fallback.value]
  return id ? mediaPath(props.selection.albumId, props.selection.imageId, id) : ''
})
function neighbor(direction: 'previous' | 'next') {
  if (props.adjacent) return props.adjacent[direction]
  const id = direction === 'previous' ? photo.value?.previous_image_id : photo.value?.next_image_id
  return id ? { albumId: props.selection.albumId, imageId: id } : undefined
}
const previous = computed(() => !loading.value && neighbor('previous'))
const next = computed(() => !loading.value && neighbor('next'))

async function load() {
  loader?.abort()
  const controller = new AbortController()
  loader = controller
  photo.value = undefined
  error.value = ''
  loading.value = true
  original.value = false
  fallback.value = 0
  const { albumId } = props.selection
  try {
    const loaded = await request<Photo>(base.value + query({
      sort: typeof route.query.sort === 'string' ? route.query.sort : undefined,
      q: typeof route.query.q === 'string' ? route.query.q : undefined,
    }), { signal: controller.signal })
    if (controller.signal.aborted) return
    photo.value = loaded
    if (account.session.actor && album.value?.id !== albumId) {
      album.value = undefined
      request<Album>(albumPath(albumId), { signal: controller.signal })
        .then(value => { if (!controller.signal.aborted) album.value = value })
        .catch(() => {})
    }
  } catch (failure) {
    if (!controller.signal.aborted) error.value = errorText(failure)
  } finally {
    if (!controller.signal.aborted) loading.value = false
  }
}
function close() {
  if (closing) return
  closing = true
  if (props.returnTo) void router.replace(props.returnTo)
  else if (history.state?.galleryLightbox) router.back()
  else {
    const query = { ...route.query }
    delete query.photo
    delete query.photoAlbum
    void router.replace({ path: route.path, query })
  }
}
function navigate(target: GalleryPhotoRef | false | undefined) {
  if (!target) return
  if (props.returnTo) {
    void router.replace({ path: photoPath(target.albumId, target.imageId), query: route.query })
  } else {
    const location = galleryPhotoLink(route, target.albumId, target.imageId)
    void router.replace({ ...location, state: { galleryLightbox: history.state?.galleryLightbox === true } })
  }
}
function onKey(event: KeyboardEvent) {
  if (event.altKey || event.ctrlKey || event.metaKey) return
  if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
    event.preventDefault()
    navigate(event.key === 'ArrowLeft' ? previous.value : next.value)
  } else if (event.key === '+' || event.key === '=') zoomTo(scale.value + .5)
  else if (event.key === '-') zoomTo(scale.value - .5)
}
function imageError() {
  if (original.value) original.value = false
  else fallback.value++
}

type Point = { x: number; y: number }
const pointers = new Map<number, Point>()
let start: { point: Point; shift: Point } | undefined
let pinch: { distance: number; scale: number; center: Point; shift: Point } | undefined
let pinched = false
let lastSwipe = 0

function zoomTo(value: number, x = shift.value.x, y = shift.value.y) {
  if (!ready.value || !image.value || !stage.value) return
  scale.value = Math.max(1, Math.min(4, value))
  const bound = (offset: number, size: number, viewport: number) => {
    const limit = Math.max(0, (size * scale.value - viewport) / 2)
    return Math.max(-limit, Math.min(limit, offset))
  }
  shift.value = {
    x: bound(x, image.value.clientWidth, stage.value.clientWidth),
    y: bound(y, image.value.clientHeight, stage.value.clientHeight),
  }
}
function toggleZoom() { zoomTo(scale.value > 1 ? 1 : 2) }
function contact() {
  const [a, b] = pointers.values()
  const box = stage.value?.getBoundingClientRect()
  if (!a || !b || !box) return
  return {
    distance: Math.max(1, Math.hypot(a.x - b.x, a.y - b.y)),
    center: { x: (a.x + b.x - box.width) / 2 - box.left, y: (a.y + b.y - box.height) / 2 - box.top },
  }
}
function point(event: PointerEvent) { return { x: event.clientX, y: event.clientY } }
function pointerDown(event: PointerEvent) {
  if (event.button !== 0 || !ready.value || pointers.size >= 2 || event.pointerType === 'mouse' && scale.value === 1) return
  const origin = point(event)
  pointers.set(event.pointerId, origin)
  ;(event.target as Element).setPointerCapture(event.pointerId)
  if (pointers.size === 1) {
    start = { point: origin, shift: shift.value }
    pinched = false
  } else {
    const touch = contact()
    if (touch) pinch = { ...touch, scale: scale.value, shift: shift.value }
    pinched = true
  }
}
function pointerMove(event: PointerEvent) {
  if (!pointers.has(event.pointerId)) return
  const current = point(event)
  pointers.set(event.pointerId, current)
  if (pinch) {
    const touch = contact()
    if (!touch) return
    const zoom = Math.max(1, Math.min(4, pinch.scale * touch.distance / pinch.distance))
    const ratio = zoom / pinch.scale
    zoomTo(zoom, touch.center.x - (pinch.center.x - pinch.shift.x) * ratio,
      touch.center.y - (pinch.center.y - pinch.shift.y) * ratio)
  } else if (start && scale.value > 1) {
    zoomTo(scale.value, start.shift.x + current.x - start.point.x, start.shift.y + current.y - start.point.y)
  }
}
function pointerUp(event: PointerEvent) {
  if (!pointers.delete(event.pointerId)) return
  pinch = undefined
  const remaining = pointers.values().next().value
  if (remaining) {
    start = { point: remaining, shift: shift.value }
    return
  }
  if (start && !pinched && scale.value === 1 && event.type === 'pointerup' && event.pointerType !== 'mouse') {
    const dx = event.clientX - start.point.x
    const dy = event.clientY - start.point.y
    if (Math.abs(dx) > 55 && Math.abs(dx) > Math.abs(dy)) {
      lastSwipe = performance.now()
      navigate(dx < 0 ? next.value : previous.value)
    } else if (dy > 75 && dy > Math.abs(dx)) close()
  }
  start = undefined
}
function closeFromBackdrop() {
  if (scale.value === 1 && !pinched && (!lastSwipe || performance.now() - lastSwipe > 350)) close()
}

watch(() => [props.selection.albumId, props.selection.imageId, route.query.sort, route.query.q], () => void load(), { immediate: true })
watch(source, () => {
  ready.value = false
  scale.value = 1
  shift.value = { x: 0, y: 0 }
  pointers.clear()
  pinch = undefined
  start = undefined
})
onMounted(() => dialog.value?.showModal())
onBeforeUnmount(() => { loader?.abort(); dialog.value?.close() })
</script>

<template>
  <Teleport to="body">
    <dialog ref="dialog" class="public-lightbox" aria-label="照片查看器" @cancel.prevent="close" @keydown="onKey">
      <header class="lightbox-top">
        <SButton autofocus size="sm" variant="ghost" aria-label="关闭查看器" @click="close"><X :size="24" aria-hidden="true" /></SButton>
        <div v-if="photo && album?.id === selection.albumId" class="lightbox-extra">
          <SButton v-if="album.capabilities.view_original && photo.can_inline_original" size="sm" variant="ghost" :aria-pressed="original"
            :aria-label="original ? '返回预览' : '查看原图'" @click="original = !original"><Scan :size="20" aria-hidden="true" /></SButton>
          <a v-if="album.capabilities.download" :href="`/api/v1${base}/original-download`" class="lightbox-download" aria-label="下载原图"><ArrowDownToLine :size="20" aria-hidden="true" /></a>
        </div>
      </header>
      <div ref="stage" class="lightbox-stage" :class="{ 'is-zoomed': scale > 1 }" @click.self="closeFromBackdrop"
        @pointerdown="pointerDown" @pointermove="pointerMove" @pointerup="pointerUp" @pointercancel="pointerUp">
        <img v-if="source" ref="image" :key="source" :src="source" :alt="caption?.text || '照片'" :class="{ 'is-ready': ready }"
          :style="{ transform: `translate(${shift.x}px, ${shift.y}px) scale(${scale})` }" draggable="false"
          @load="ready = true" @error="imageError" @dblclick.prevent="toggleZoom" />
        <div v-if="loading || source && !ready" class="lightbox-status" role="status"><SSpinner /><span class="sr-only">加载照片</span></div>
        <div v-else-if="!source" class="lightbox-status" role="status">
          <ImageOff :size="27" aria-hidden="true" /><span>{{ error || '预览暂不可用' }}</span>
          <SButton size="sm" variant="secondary" @click="load">重试</SButton>
        </div>
      </div>
      <footer class="lightbox-footer">
        <p v-if="caption?.text" class="lightbox-caption">{{ caption.text }}</p>
        <div class="lightbox-controls">
          <SButton class="lightbox-prev" variant="ghost" aria-label="上一张" :disabled="!previous" @click="navigate(previous)">
            <ChevronLeft :size="24" aria-hidden="true" />
          </SButton>
          <SButton variant="ghost" :aria-label="scale > 1 ? '适合屏幕' : '放大'" :aria-pressed="scale > 1" :disabled="!ready" @click="toggleZoom">
            <ZoomOut v-if="scale > 1" :size="21" aria-hidden="true" /><ZoomIn v-else :size="21" aria-hidden="true" />
          </SButton>
          <SButton class="lightbox-next" variant="ghost" aria-label="下一张" :disabled="!next" @click="navigate(next)">
            <ChevronRight :size="24" aria-hidden="true" />
          </SButton>
        </div>
      </footer>
    </dialog>
  </Teleport>
</template>
