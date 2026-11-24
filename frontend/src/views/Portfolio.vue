<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute } from 'vue-router'
import { SButton, SSpinner } from '@syntrofic/ui'
import { Image as ImageIcon } from 'lucide-vue-next'
import { account, albumPath, errorText, mediaPath, query, request, type Page, type Collection, type Work } from '../api'
import { galleryPhoto, galleryPhotoLink } from '../galleryPhoto'
import { fittedVariants, mediaSrcset, photoAspectRatio } from '../galleryMedia'
import Notice from '../components/Notice.vue'
import PublicLightbox from '../components/PublicLightbox.vue'
import PublicPhotoGrid from '../components/PublicPhotoGrid.vue'

const route = useRoute()
const isWorks = computed(() => route.path === '/photos')
const filter = computed(() => typeof route.query.album === 'string' ? route.query.album : '')
const collections = ref<Collection[]>([])
const works = ref<Work[]>([])
const cursor = ref<string | null>(null)
const loading = ref(true)
const error = ref('')
const failed = ref(new Set<string>())
const activeCollection = computed(() => collections.value.find(album => album.id === filter.value))
const selection = computed(() => galleryPhoto(route))
const selectedIndex = computed(() => isWorks.value && selection.value ? works.value.findIndex(
  item => item.id === selection.value?.imageId && item.album_id === selection.value?.albumId,
) : -1)
const featured = computed(() => {
  const covers = collections.value.flatMap(collection => collection.covers)
    .filter(work => media(work, true).src)
  return covers.find(work => photoAspectRatio(work) >= 1) ?? covers[0]
})
const heroMedia = computed(() => featured.value && media(featured.value, true))
let requests: AbortController | undefined

function media(work: Work, large = false) {
  const variants = work.variants.filter(item => !failed.value.has(item.id))
  const fitted = fittedVariants(variants, photoAspectRatio(work))
  const preferred = fitted.filter(item => large ? item.spec_id !== 'thumb' : item.spec_id === 'thumb' || item.spec_id === 'mobile')
  const candidates = preferred.length ? preferred : fitted.length ? fitted : variants
  const variant = candidates.find(item => item.spec_id === (large ? 'mobile' : 'thumb')) || candidates[0]
  return {
    src: variant ? mediaPath(work.album_id, work.id, variant.id) : '',
    srcset: mediaSrcset(candidates, work.album_id, work.id),
  }
}
function markFailed(event: Event) {
  const image = event.currentTarget as HTMLImageElement
  const match = (image.currentSrc || image.src).match(/\/variants\/([^/]+)\/body$/)
  if (match?.[1]) failed.value.add(decodeURIComponent(match[1]))
}
function albumLink(id: string) { return { path: albumPath(id), query: { portfolio: '1' } } }
function workLink(work: Work) { return galleryPhotoLink(route, work.album_id, work.id) }
async function load(append = false) {
  requests?.abort()
  const current = new AbortController()
  requests = current
  loading.value = true
  error.value = ''
  try {
    const options = { signal: current.signal }
    if (!append) {
      const groups = await request<{ items: Collection[] }>('/public/collections', options)
      if (current.signal.aborted) return
      collections.value = groups.items
      works.value = []
      cursor.value = null
    }
    if (isWorks.value) {
      const page = await request<Page<Work>>('/public/works' + query({
        album_id: filter.value, cursor: append ? cursor.value : undefined, limit: 40,
      }), options)
      if (current.signal.aborted) return
      works.value = append ? [...works.value, ...page.items] : page.items
      cursor.value = page.next_cursor
    }
  } catch (failure) {
    if (!current.signal.aborted) error.value = errorText(failure)
  } finally {
    if (!current.signal.aborted) loading.value = false
  }
}
watch([isWorks, filter], () => void load())
watch([selectedIndex, loading], ([index, pending]) => {
  if (!pending && !error.value && cursor.value && index >= 0 && index >= works.value.length - 2) void load(true)
})
onMounted(() => void load())
onUnmounted(() => requests?.abort())
</script>

<template>
  <section v-if="!isWorks" class="portfolio-home">
    <header class="portfolio-content portfolio-hero" :class="{ 'has-featured': featured }">
      <div class="portfolio-intro">
        <h1>{{ account.presentation.title }}</h1>
        <p v-if="account.presentation.description" class="preserve">{{ account.presentation.description }}</p>
        <RouterLink v-if="featured" to="/photos" class="portfolio-link">浏览全部作品</RouterLink>
      </div>
      <RouterLink v-if="featured && heroMedia" :to="workLink(featured)" class="portfolio-feature" :aria-label="featured.display_name">
        <img v-bind="heroMedia" sizes="(max-width: 640px) calc(100vw - 1.8rem), 60vw" alt="" fetchpriority="high" decoding="async" @error="markFailed" />
      </RouterLink>
    </header>
    <div class="portfolio-content">
      <div class="portfolio-section-title"><h2>相册</h2></div>
      <Notice :error="error" />
      <div v-if="loading && !collections.length" class="loading" role="status"><SSpinner /> 正在读取相册</div>
      <div v-else-if="collections.length" class="portfolio-collections">
        <article v-for="collection in collections" :key="collection.id" class="collection">
          <RouterLink :to="albumLink(collection.id)" class="collection-link">
            <div class="collection-stack">
              <div v-for="(work, index) in collection.covers.filter(item => media(item).src)" :key="work.id"
                class="collection-cover" :class="`cover-${index}`">
                <img v-bind="media(work)" sizes="(max-width: 640px) 46vw, 30vw" alt="" loading="lazy" decoding="async" @error="markFailed" />
              </div>
              <div v-if="!collection.covers.some(item => media(item).src)" class="collection-cover cover-0 empty-cover"><ImageIcon :size="27" :stroke-width="1.25" aria-hidden="true" /></div>
            </div>
            <div class="collection-meta"><h3>{{ collection.name }}</h3><span>{{ collection.count }} 张</span></div>
            <p v-if="collection.description" class="collection-description">{{ collection.description }}</p>
          </RouterLink>
        </article>
      </div>
      <div v-else-if="!error && !loading" class="portfolio-empty"><ImageIcon :size="28" :stroke-width="1.2" aria-hidden="true" /><p>暂时还没有公开相册。</p></div>
      <SButton v-if="error" variant="secondary" @click="load()">重试</SButton>
    </div>
  </section>

  <section v-else class="portfolio-content portfolio-works">
    <div class="portfolio-section-title">
      <div><h1>{{ activeCollection?.name || '全部作品' }}</h1></div>
    </div>
    <Notice :error="error" />
    <PublicPhotoGrid v-if="works.length" :photos="works" />
    <div v-if="loading" class="loading" role="status"><SSpinner /> 正在读取作品</div>
    <div v-else-if="!works.length && !error" class="portfolio-empty"><ImageIcon :size="28" :stroke-width="1.2" aria-hidden="true" /><p>暂时还没有公开作品。</p></div>
    <div v-if="cursor || error" class="portfolio-more"><SButton variant="secondary" :loading="loading" @click="load(Boolean(cursor))">{{ error ? '重试' : '加载更多' }}</SButton></div>
  </section>
  <PublicLightbox v-if="selection" :selection="selection" :photos="isWorks ? works : featured ? [featured] : undefined" :follow-list-order="isWorks" />
</template>
