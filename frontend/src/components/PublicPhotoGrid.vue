<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { RouterLink, useRoute } from 'vue-router'
import { Image as ImageIcon } from 'lucide-vue-next'
import { mediaPath, type Photo } from '../api'
import { fittedVariants, mediaSrcset, photoAspectRatio } from '../galleryMedia'
import { galleryPhotoLink } from '../galleryPhoto'

type GalleryImage = Photo & { album_id?: string }
const props = defineProps<{ photos: readonly GalleryImage[]; albumId?: string }>()
const route = useRoute()
const grid = ref<HTMLElement>()
const width = ref(0)
const failed = ref(new Set<string>())
const gap = 4
let observer: ResizeObserver | undefined

const layout = computed(() => {
  if (!width.value) return { count: 2, height: 0, tiles: [] }
  const count = Math.max(2, Math.min(7, Math.floor((width.value + gap) / (260 + gap))))
  const columnWidth = (width.value - gap * (count - 1)) / count
  const heights = Array<number>(count).fill(0)
  const tiles = props.photos.map((photo, index) => {
    const column = heights.indexOf(Math.min(...heights))
    const ratio = photoAspectRatio(photo)
    const height = Math.max(1, Math.round(columnWidth / ratio))
    const left = column * (columnWidth + gap)
    const top = heights[column]
    heights[column] += height + gap
    return { photo, index, ratio, left, top, width: columnWidth, height }
  })
  return { count, height: Math.max(0, Math.max(...heights) - gap), tiles }
})

function album(photo: GalleryImage) { return props.albumId || photo.album_id! }
function preview(photo: GalleryImage, ratio: number) {
  const available = (photo.variants || []).filter(variant => !failed.value.has(variant.id))
  const fitted = fittedVariants(available, ratio)
  const small = fitted.filter(variant => variant.spec_id === 'thumb' || variant.spec_id === 'mobile')
  const candidates = small.length ? small : fitted.length ? fitted : available
  const best = candidates.find(variant => variant.spec_id === 'thumb') || candidates[0]
  return {
    src: best ? mediaPath(album(photo), photo.id, best.id) : '',
    srcset: mediaSrcset(small.length ? small : fitted, album(photo), photo.id),
  }
}
const tiles = computed(() => layout.value.tiles.map(tile => ({ ...tile, ...preview(tile.photo, tile.ratio) })))
function markFailed(event: Event) {
  const image = event.currentTarget as HTMLImageElement
  const variant = (image.currentSrc || image.src).match(/\/variants\/([^/]+)\/body$/)?.[1]
  if (variant) failed.value.add(decodeURIComponent(variant))
}

onMounted(() => {
  if (!grid.value) return
  width.value = grid.value.clientWidth
  observer = new ResizeObserver(([entry]) => { width.value = entry.contentRect.width })
  observer.observe(grid.value)
})
onUnmounted(() => observer?.disconnect())
</script>

<template>
  <div ref="grid" class="photo-wall" :data-columns="layout.count" :style="{ height: `${layout.height}px` }">
    <RouterLink v-for="tile in tiles" :key="tile.photo.id"
      :to="galleryPhotoLink(route, album(tile.photo), tile.photo.id)" :aria-label="tile.photo.display_name" class="photo-wall-item"
      :style="{ left: `${tile.left}px`, top: `${tile.top}px`, width: `${tile.width}px`, height: `${tile.height}px` }">
      <img v-if="tile.src" :src="tile.src" :srcset="tile.srcset" :sizes="`${Math.ceil(tile.width)}px`"
        :loading="tile.index < layout.count * 2 ? 'eager' : 'lazy'" alt="" decoding="async" @error="markFailed" />
      <ImageIcon v-else :size="24" :stroke-width="1.25" aria-hidden="true" />
    </RouterLink>
  </div>
</template>
