<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { SBadge, SButton, SDialog, SSpinner, STextArea } from '@syntrofic/ui'
import { ArrowDownToLine, ChevronLeft, ChevronRight, ImageOff, Scan, Trash2, X, ZoomIn } from 'lucide-vue-next'
import { account, albumPath, errorText, mediaPath, pathSegment, photoPath, query, request, visibleCaption, type Album, type Photo } from '../api'
import Notice from '../components/Notice.vue'
import PublicLightbox from '../components/PublicLightbox.vue'

const route = useRoute()
const router = useRouter()
const album = ref<Album>()
const photo = ref<Photo>()
const caption = computed(() => visibleCaption(photo.value))
const id = computed(() => String(route.params.imageId))
const albumId = computed(() => route.params.albumId ? String(route.params.albumId) : undefined)
const base = computed(() => albumId.value ? photoPath(albumId.value, id.value) : `/library/images/${pathSegment(id.value)}`)
const publicView = computed(() => route.query.portfolio === '1' || !account.session.actor)
const publicSelection = computed(() => publicView.value && albumId.value ? { albumId: albumId.value, imageId: id.value } : undefined)
const parent = computed(() => {
  if (route.query.from === 'home') return { path: '/' }
  if (route.query.from === 'works') return { path: '/photos', query: route.query.album ? { album: route.query.album } : {} }
  return albumId.value ? { path: albumPath(albumId.value), query: route.query } : { path: '/library' }
})
const owner = computed(() => !publicView.value && account.session.actor?.role === 'owner')
const canDownload = computed(() => album.value?.capabilities.download ?? owner.value)
const canView = computed(() => album.value?.capabilities.view_original ?? owner.value)
const error = ref('')
const message = ref('')
const loading = ref(true)
const original = ref(false)
const zoom = ref(false)
const fallback = ref(0)
const editing = ref(false)
const trashOpen = ref(false)
const draft = ref('')
const busy = ref(false)
let loader: AbortController | undefined
const choices = computed(() => {
  if (!photo.value) return []
  const priority = photo.value.selection?.desktop?.variant_id
  return Array.from(new Set([priority, ...(photo.value.variants || []).map(item => item.id)].filter((item): item is string => Boolean(item))))
})
const imageUrl = computed(() => {
  if (!photo.value) return ''
  if (original.value) return `/api/v1${base.value}/original-view`
  const choice = choices.value[fallback.value]
  return choice ? mediaPath(albumId.value, id.value, choice) : ''
})
const srcset = computed(() => !original.value && !fallback.value ? photo.value?.srcset?.filter(item => item.width)
  .map(item => `${mediaPath(albumId.value, id.value, item.variant_id)} ${item.width}w`).join(', ') : undefined)

async function load(quiet = false) {
  loader?.abort()
  const current = new AbortController()
  loader = current
  if (!quiet) {
    photo.value = undefined
    error.value = ''
    loading.value = true
    original.value = false
    zoom.value = false
    fallback.value = 0
  }
  try {
    const [image, context] = await Promise.all([
      request<Photo>(base.value + query({ sort: route.query.sort as string | undefined, q: route.query.q as string | undefined }), { signal: current.signal }),
      albumId.value ? request<Album>(albumPath(albumId.value), { signal: current.signal }) : undefined,
    ])
    if (current.signal.aborted) return
    photo.value = image
    album.value = context
    if ((context && !context.capabilities.view_original) || (context && !image.can_inline_original)) original.value = false
  } catch (failure) {
    if (!current.signal.aborted) {
      error.value = errorText(failure)
      photo.value = undefined
      original.value = false
    }
  } finally { if (!current.signal.aborted) loading.value = false }
}
function navigate(target?: string | null) {
  if (target && albumId.value) void router.replace({ path: photoPath(albumId.value, target), query: route.query })
}
function onKey(event: KeyboardEvent) {
  if (publicSelection.value || editing.value || event.target instanceof HTMLInputElement || event.target instanceof HTMLTextAreaElement) return
  if (event.key === 'ArrowLeft' && photo.value?.previous_image_id) { event.preventDefault(); navigate(photo.value.previous_image_id) }
  if (event.key === 'ArrowRight' && photo.value?.next_image_id) { event.preventDefault(); navigate(photo.value.next_image_id) }
  if (event.key === 'Escape') void router.replace(parent.value)
}
function imageError() {
  if (original.value) {
    original.value = false
    error.value = '原图暂时无法打开。'
    void load(true)
  } else fallback.value++
}
async function saveCaption() {
  busy.value = true
  error.value = ''
  try {
    const resource = `/images/${pathSegment(id.value)}/caption`
    const caption = await request<{ revision: number }>(resource, { method: 'PATCH', body: { text: draft.value }, revision: photo.value?.revision })
    if (caption) message.value = '照片描述已保存。'
    editing.value = false
    await load(true)
  } catch (failure) { error.value = errorText(failure) }
  finally { busy.value = false }
}
async function moveToTrash() {
  if (!photo.value?.lifecycle_revision) return
  busy.value = true
  error.value = ''
  try {
    await request(`/images/${pathSegment(id.value)}/trash`, {
      method: 'POST', body: { expected_lifecycle_revision: photo.value.lifecycle_revision },
    })
    trashOpen.value = false
    await router.replace(parent.value)
  } catch (failure) { error.value = errorText(failure) }
  finally { busy.value = false }
}
watch([() => route.path, publicSelection], () => { if (!publicSelection.value) void load() })
watch(() => account.event, () => { if (!publicSelection.value && !editing.value && !busy.value) void load(true) })
onMounted(() => { if (!publicSelection.value) void load(); window.addEventListener('keydown', onKey) })
onUnmounted(() => { window.removeEventListener('keydown', onKey); loader?.abort() })
</script>

<template>
  <PublicLightbox v-if="publicSelection" :selection="publicSelection" :return-to="parent" />
  <section v-else class="viewer stack">
    <div class="viewer-top">
      <RouterLink :to="parent" class="inline-action"><X :size="18" aria-hidden="true" />关闭查看器</RouterLink>
      <div class="actions" v-if="photo">
        <SButton v-if="canView && photo.can_inline_original" variant="secondary" :aria-pressed="original" @click="original = !original">
          <Scan :size="16" aria-hidden="true" />{{ original ? '返回预览' : '查看原图' }}
        </SButton>
        <a v-if="canDownload" :href="`/api/v1${base}/original-download`" download class="inline-action"><ArrowDownToLine :size="17" aria-hidden="true" />下载原图</a>
      </div>
    </div>
    <Notice :error="error" :message="message" />
    <div v-if="loading" class="loading" role="status"><SSpinner /> 加载照片</div>
    <template v-if="photo">
      <div class="viewer-stage" :class="{ zoomed: zoom }">
        <img v-if="imageUrl" :srcset="srcset" :src="imageUrl" sizes="(max-width: 759px) 100vw, calc(100vw - 285px)"
          :alt="caption?.source === 'human' ? caption.text : publicView ? '照片' : photo.display_name" @error="imageError" />
        <div v-else class="viewer-placeholder"><ImageOff :size="38" :stroke-width="1.25" /><p>预览尚未生成</p></div>
      </div>
      <div class="viewer-caption">
        <div>
          <h1 v-if="!publicView">{{ photo.display_name }}</h1>
          <p class="page-meta">{{ photo.width }} × {{ photo.height }} 像素</p>
        </div>
        <div class="actions">
          <SButton v-if="owner" variant="ghost" aria-label="移入回收站" @click="trashOpen = true"><Trash2 :size="18" /></SButton>
          <SButton v-if="imageUrl" variant="ghost" :aria-pressed="zoom" @click="zoom = !zoom"><ZoomIn :size="17" />{{ zoom ? '适合屏幕' : '原尺寸' }}</SButton>
          <template v-if="albumId">
            <SButton variant="secondary" :disabled="!photo.previous_image_id" aria-label="上一张" @click="navigate(photo.previous_image_id)"><ChevronLeft :size="20" /></SButton>
            <SButton variant="secondary" :disabled="!photo.next_image_id" aria-label="下一张" @click="navigate(photo.next_image_id)"><ChevronRight :size="20" /></SButton>
          </template>
        </div>
      </div>
      <div v-if="caption || owner || (!publicView && album?.capabilities.edit)" class="panel form-stack">
        <div class="title-row"><h2>照片描述</h2><SButton v-if="owner || (!publicView && album?.capabilities.edit)" variant="secondary" @click="draft = caption?.text || ''; editing = true">编辑描述</SButton></div>
        <SBadge v-if="caption?.source === 'model'" class="fit">AI 生成</SBadge>
        <p v-if="caption" class="preserve">{{ caption.text }}</p>
      </div>
      <SDialog v-model:open="trashOpen" title="移入回收站" dismiss-label="关闭">
        <div class="form-stack"><p>照片会从所有相册中隐藏，可在回收站恢复。</p><Notice :error="error" />
          <div class="actions"><SButton variant="secondary" @click="trashOpen = false">取消</SButton><SButton variant="danger" :loading="busy" @click="moveToTrash">移入回收站</SButton></div>
        </div>
      </SDialog>
      <SDialog v-model:open="editing" title="编辑照片描述" dismiss-label="关闭">
        <form class="form-stack" @submit.prevent="saveCaption">
          <STextArea v-model="draft" label="描述" :rows="4" />
          <Notice :error="error" />
          <SButton type="submit" :loading="busy">保存描述</SButton>
        </form>
      </SDialog>
    </template>
  </section>
</template>
