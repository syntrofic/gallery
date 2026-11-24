<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { useRoute, useRouter, type RouteLocationRaw } from 'vue-router'
import { SButton } from '@syntrofic/ui'
import { ArrowDownToLine, ChevronLeft, ChevronRight, Scan, X, ZoomIn, ZoomOut } from 'lucide-vue-next'
import { account, albumPath, photoPath, request, visibleCaption, type Album, type Photo } from '../api'
import { galleryPhotoLink, type GalleryPhotoRef } from '../galleryPhoto'
import { downloadPhotoPreview } from '../photoDownload'
import PublicLightboxSlide from './PublicLightboxSlide.vue'

type ListedPhoto = Photo & { album_id?: string }
type Point = { x: number; y: number }
type Preview = { zoomable: boolean; settled: boolean; url: string; width: number; height: number }

const props = defineProps<{
  selection: GalleryPhotoRef
  photos?: readonly ListedPhoto[]
  followListOrder?: boolean
  returnTo?: RouteLocationRaw
}>()
const route = useRoute()
const router = useRouter()
const dialog = ref<HTMLDialogElement>()
const stage = ref<HTMLElement>()
const track = ref<HTMLElement>()
const viewport = ref({ width: 0, height: 0 })
const zoom = ref({ scale: 1, x: 0, y: 0 })
const resolutionScale = ref(1)
const detail = ref<Photo>()
const preview = ref<Preview>({ zoomable: false, settled: false, url: '', width: 0, height: 0 })
const album = ref<Album>()
const original = ref(false)
const notice = ref('')
const downloading = ref(false)
const settling = ref(false)
let observer: ResizeObserver | undefined
let transfer: AbortController | undefined
let motion: Animation | undefined
let closing = false

function key(target: GalleryPhotoRef) { return JSON.stringify([target.albumId, target.imageId]) }
const activeKey = computed(() => key(props.selection))
const base = computed(() => photoPath(props.selection.albumId, props.selection.imageId))
const sort = computed(() => typeof route.query.sort === 'string' ? route.query.sort : undefined)
const index = computed(() => props.photos?.findIndex(item =>
  item.id === props.selection.imageId && (item.album_id ?? props.selection.albumId) === props.selection.albumId) ?? -1)
const current = computed(() => detail.value ?? props.photos?.[index.value])
const caption = computed(() => visibleCaption(current.value))
const access = computed(() => album.value?.id === props.selection.albumId ? album.value.capabilities : undefined)

function neighbor(direction: -1 | 1) {
  const listed = index.value >= 0 ? props.photos?.[index.value + direction] : undefined
  if (listed) return { albumId: listed.album_id ?? props.selection.albumId, imageId: listed.id }
  if (props.followListOrder && index.value >= 0) return
  const id = direction < 0 ? detail.value?.previous_image_id : detail.value?.next_image_id
  if (id) return { albumId: props.selection.albumId, imageId: id }
}
const previous = computed(() => neighbor(-1))
const next = computed(() => neighbor(1))
const slides = computed(() => [previous.value, props.selection, next.value].flatMap((target, position) => {
  if (!target) return []
  const offset = position - 1
  const listed = props.photos?.[index.value + offset]
  const seed = listed?.id === target.imageId && (listed.album_id ?? props.selection.albumId) === target.albumId ? listed : undefined
  return [{ target, offset, seed, key: key(target) }]
}))

watch([() => props.selection.albumId, () => account.session.actor?.id], ([id, actor], _, onCleanup) => {
  album.value = undefined
  if (!actor) return
  const controller = new AbortController()
  onCleanup(() => controller.abort())
  request<Album>(albumPath(id), { signal: controller.signal })
    .then(value => { if (!controller.signal.aborted) album.value = value })
    .catch(() => {})
}, { immediate: true })

function acceptDetail(id: string, value?: Photo) {
  if (id === activeKey.value) detail.value = value
}
function acceptPreview(id: string, value: Preview) {
  if (id === activeKey.value) preview.value = value
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
function navigate(target: GalleryPhotoRef) {
  if (props.returnTo) return router.replace({ path: photoPath(target.albumId, target.imageId), query: route.query })
  const location = galleryPhotoLink(route, target.albumId, target.imageId)
  return router.replace({ ...location, state: { galleryLightbox: history.state?.galleryLightbox === true } })
}
function onKey(event: KeyboardEvent) {
  if (event.altKey || event.ctrlKey || event.metaKey) return
  if (event.key === 'ArrowLeft' || event.key === 'ArrowRight') {
    event.preventDefault()
    void step(event.key === 'ArrowLeft' ? -1 : 1)
  } else if (event.key === '+' || event.key === '=') zoomTo(zoom.value.scale + .5)
  else if (event.key === '-') zoomTo(zoom.value.scale - .5)
}

async function savePreview() {
  if (!preview.value.url || !current.value || downloading.value) return
  const id = activeKey.value
  const controller = new AbortController()
  transfer = controller
  downloading.value = true
  notice.value = ''
  const timer = window.setTimeout(() => controller.abort(), 30_000)
  try {
    await downloadPhotoPreview(preview.value.url, current.value.display_name, controller.signal)
    if (id === activeKey.value) notice.value = '已交给浏览器下载'
  } catch {
    if (!closing && id === activeKey.value) notice.value = '下载失败，请重试。'
  } finally {
    window.clearTimeout(timer)
    downloading.value = false
  }
}

function zoomTo(value: number, x = zoom.value.x, y = zoom.value.y, updateResolution = true) {
  if (!preview.value.zoomable || settling.value) return
  const scale = Math.max(1, Math.min(4, value))
  const limitX = Math.max(0, (preview.value.width * scale - viewport.value.width) / 2)
  const limitY = Math.max(0, (preview.value.height * scale - viewport.value.height) / 2)
  zoom.value = { scale, x: Math.max(-limitX, Math.min(limitX, x)), y: Math.max(-limitY, Math.min(limitY, y)) }
  if (updateResolution) resolutionScale.value = scale
}
function toggleZoom() { zoomTo(zoom.value.scale > 1 ? 1 : 2) }

const pointers = new Map<number, Point>()
let start: { point: Point; pan: Point; time: number } | undefined
let pinch: { distance: number; scale: number; center: Point; pan: Point } | undefined
let mode: 'pending' | 'horizontal' | 'vertical' | 'pan' | 'blocked' = 'pending'
let lastTap: { point: Point; time: number } | undefined
let lastGesture = -Infinity
let drag = { x: 0, y: 0 }
let frame: number | undefined

function transform({ x, y }: Point) {
  const shrink = 1 - Math.min(y / Math.max(1, viewport.value.height) * .2, .08)
  return `translate3d(${x}px, ${y}px, 0) scale(${shrink})`
}
function paintDrag() {
  if (frame !== undefined) cancelAnimationFrame(frame)
  frame = undefined
  if (track.value) track.value.style.transform = transform(drag)
}
function setDrag(x: number, y: number) {
  drag = { x, y }
  if (frame === undefined) frame = requestAnimationFrame(paintDrag)
}
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
  if (settling.value || event.button !== 0 || pointers.size >= 2 ||
    (event.target as Element).closest('button') || event.pointerType === 'mouse' && zoom.value.scale === 1) return
  const origin = point(event)
  pointers.set(event.pointerId, origin)
  stage.value?.setPointerCapture(event.pointerId)
  if (pointers.size === 1) {
    start = { point: origin, pan: zoom.value, time: performance.now() }
    mode = 'pending'
  } else {
    const pair = contact()
    if (pair) pinch = { ...pair, scale: zoom.value.scale, pan: zoom.value }
    setDrag(0, 0)
    mode = 'blocked'
  }
}
function pointerMove(event: PointerEvent) {
  if (!pointers.has(event.pointerId)) return
  const position = point(event)
  pointers.set(event.pointerId, position)
  if (pinch) {
    const pair = contact()
    if (!pair) return
    const scale = Math.max(1, Math.min(4, pinch.scale * pair.distance / pinch.distance))
    const ratio = scale / pinch.scale
    zoomTo(scale, pair.center.x - (pinch.center.x - pinch.pan.x) * ratio,
      pair.center.y - (pinch.center.y - pinch.pan.y) * ratio, false)
    return
  }
  if (!start) return
  const x = position.x - start.point.x
  const y = position.y - start.point.y
  if (mode === 'pending' && Math.hypot(x, y) > 7) {
    if (zoom.value.scale > 1) mode = 'pan'
    else if (Math.abs(x) > Math.abs(y)) mode = 'horizontal'
    else mode = y > 0 ? 'vertical' : 'blocked'
  }
  if (mode === 'horizontal') {
    const target = x > 0 ? previous.value : next.value
    const limit = viewport.value.width
    setDrag(Math.max(-limit, Math.min(limit, target ? x : x * .2)), 0)
  } else if (mode === 'vertical') setDrag(0, Math.max(0, y))
  else if (mode === 'pan') zoomTo(zoom.value.scale, start.pan.x + x, start.pan.y + y, false)
}
async function settle(to: Point, duration: number, finish?: () => void | Promise<unknown>) {
  if (settling.value || closing) return
  settling.value = true
  paintDrag()
  const element = track.value
  try {
    if (element && !matchMedia('(prefers-reduced-motion: reduce)').matches) {
      motion = element.animate([{ transform: transform(drag) }, { transform: transform(to) }],
        { duration, easing: 'cubic-bezier(.22,.8,.2,1)', fill: 'forwards' })
      await motion.finished
    }
    await finish?.()
    await nextTick()
  } catch { /* The animation may be cancelled when the viewer closes. */ }
  finally {
    drag = { x: 0, y: 0 }
    paintDrag()
    motion?.cancel()
    motion = undefined
    settling.value = false
  }
}
function step(direction: -1 | 1, velocity = 0) {
  const target = direction < 0 ? previous.value : next.value
  if (!target || pointers.size || settling.value) return
  const id = activeKey.value
  const width = viewport.value.width
  const duration = Math.max(150, 250 - Math.min(1, Math.abs(drag.x) / Math.max(1, width)) * 75 - Math.abs(velocity) * 35)
  return settle({ x: -direction * width, y: 0 }, duration, () => {
    if (id === activeKey.value && !closing) return navigate(target)
  })
}
function pointerUp(event: PointerEvent) {
  if (!pointers.delete(event.pointerId)) return
  const remaining = pointers.values().next().value
  if (pinch) {
    pinch = undefined
    mode = zoom.value.scale > 1 ? 'pan' : 'blocked'
    if (remaining) start = { point: remaining, pan: zoom.value, time: performance.now() }
  }
  if (remaining) return
  resolutionScale.value = zoom.value.scale
  const ended = start
  start = undefined
  if (!ended) return
  const now = performance.now()
  const x = event.clientX - ended.point.x
  const y = event.clientY - ended.point.y
  const elapsed = Math.max(1, now - ended.time)
  if (mode !== 'pending') lastGesture = now
  if (event.type === 'pointerup') {
    if (mode === 'horizontal' && (Math.abs(x) > viewport.value.width * .17 || Math.abs(x) > 22 && Math.abs(x) / elapsed > .38)) {
      const direction = x < 0 ? 1 : -1
      if (direction < 0 ? previous.value : next.value) {
        void step(direction, x / elapsed)
        return
      }
    } else if (mode === 'vertical' && (y > viewport.value.height * .16 || y > 28 && y / elapsed > .45)) {
      void settle({ x: 0, y: viewport.value.height }, 180, close)
      return
    } else if (mode === 'pending' && event.pointerType !== 'mouse') {
      if (lastTap && now - lastTap.time < 300 && Math.hypot(event.clientX - lastTap.point.x, event.clientY - lastTap.point.y) < 28) {
        lastTap = undefined
        lastGesture = now
        toggleZoom()
      } else lastTap = { point: point(event), time: now }
    }
  }
  if (drag.x || drag.y) void settle({ x: 0, y: 0 }, 180)
}
function doubleClick(event: MouseEvent) {
  if (performance.now() - lastGesture > 350 && (event.target as Element).closest('.lightbox-picture')) toggleZoom()
}
function backdrop(event: MouseEvent) {
  if (zoom.value.scale === 1 && !settling.value && performance.now() - lastGesture > 350 &&
    (event.target as Element).classList.contains('lightbox-slide')) close()
}

watch([activeKey, sort], () => {
  detail.value = undefined
  preview.value = { zoomable: false, settled: false, url: '', width: 0, height: 0 }
  zoom.value = { scale: 1, x: 0, y: 0 }
  resolutionScale.value = 1
  original.value = false
  notice.value = ''
  pointers.clear()
  start = pinch = lastTap = undefined
  if (!settling.value && (drag.x || drag.y)) {
    drag = { x: 0, y: 0 }
    paintDrag()
  }
}, { flush: 'sync' })
onMounted(() => {
  dialog.value?.showModal()
  if (!stage.value) return
  viewport.value = { width: stage.value.clientWidth, height: stage.value.clientHeight }
  observer = new ResizeObserver(([entry]) => {
    viewport.value = { width: entry.contentRect.width, height: entry.contentRect.height }
  })
  observer.observe(stage.value)
})
onBeforeUnmount(() => {
  closing = true
  transfer?.abort()
  observer?.disconnect()
  if (frame !== undefined) cancelAnimationFrame(frame)
  motion?.cancel()
  dialog.value?.close()
})
</script>

<template>
  <Teleport to="body">
    <dialog ref="dialog" class="public-lightbox" aria-label="照片查看器" @cancel.prevent="close" @keydown="onKey">
      <header class="lightbox-top">
        <SButton autofocus size="sm" variant="ghost" aria-label="关闭查看器" @click="close"><X :size="24" aria-hidden="true" /></SButton>
        <div class="lightbox-extra">
          <SButton v-if="access?.view_original && detail?.can_inline_original" size="sm" variant="ghost" :aria-pressed="original"
            :aria-label="original ? '返回预览' : '查看原图'" @click="original = !original"><Scan :size="20" aria-hidden="true" /></SButton>
          <a v-if="access?.download && detail" :href="`/api/v1${base}/original-download`" download class="lightbox-download" aria-label="下载原图">
            <ArrowDownToLine :size="20" aria-hidden="true" />
          </a>
          <SButton v-else-if="preview.url || downloading" size="sm" variant="ghost" aria-label="下载照片"
            :loading="downloading" @click="savePreview"><ArrowDownToLine :size="20" aria-hidden="true" /></SButton>
        </div>
        <p v-if="notice" class="lightbox-notice" role="status">{{ notice }}</p>
      </header>
      <div ref="stage" class="lightbox-stage" :class="{ 'is-zoomed': zoom.scale > 1 }" @click="backdrop" @dblclick.prevent="doubleClick"
        @pointerdown="pointerDown" @pointermove="pointerMove" @pointerup="pointerUp" @pointercancel="pointerUp" @lostpointercapture="pointerUp">
        <div ref="track" class="lightbox-track">
          <PublicLightboxSlide v-for="slide in slides" :key="slide.key" :selection="slide.target" :seed="slide.seed"
            :offset="slide.offset" :active="slide.offset === 0" :preload="preview.settled" :sort="sort" :viewport="viewport"
            :zoom="zoom" :resolution-scale="resolutionScale" :original="slide.offset === 0 && original"
            @detail="acceptDetail(slide.key, $event)" @preview="acceptPreview(slide.key, $event)"
            @original-error="original = false; notice = '原图暂时无法打开。'" />
        </div>
      </div>
      <footer class="lightbox-footer">
        <p v-if="caption?.text" class="lightbox-caption">{{ caption.text }}</p>
        <div class="lightbox-controls">
          <SButton class="lightbox-prev" variant="ghost" aria-label="上一张" :disabled="!previous" @click="step(-1)">
            <ChevronLeft :size="24" aria-hidden="true" />
          </SButton>
          <SButton variant="ghost" :aria-label="zoom.scale > 1 ? '适合屏幕' : '放大'" :aria-pressed="zoom.scale > 1" :disabled="!preview.zoomable" @click="toggleZoom">
            <ZoomOut v-if="zoom.scale > 1" :size="21" aria-hidden="true" /><ZoomIn v-else :size="21" aria-hidden="true" />
          </SButton>
          <SButton class="lightbox-next" variant="ghost" aria-label="下一张" :disabled="!next" @click="step(1)">
            <ChevronRight :size="24" aria-hidden="true" />
          </SButton>
        </div>
      </footer>
    </dialog>
  </Teleport>
</template>
