<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { SButton, SSpinner } from '@syntrofic/ui'
import { ChevronDown, Image as ImageIcon } from 'lucide-vue-next'
import {
  account, albumPath, errorText, mediaPath, photoPath, query, request,
  type Page, type Collection, type Work,
} from '../api'
import Notice from '../components/Notice.vue'

const route = useRoute()
const router = useRouter()
const isWorks = computed(() => route.path === '/photos')
const filter = computed(() => typeof route.query.album === 'string' ? route.query.album : '')
const collections = ref<Collection[]>([])
const works = ref<Work[]>([])
const cursor = ref<string | null>(null)
const loading = ref(true)
const error = ref('')
const width = ref(0)
const workGrid = ref<HTMLElement>()
const failed = ref(new Set<string>())
const activeCollection = computed(() => collections.value.find(album => album.id === filter.value))
const featured = computed(() => {
  const covers = collections.value.flatMap(collection => collection.covers)
    .filter(work => media(work, 'desktop'))
  return covers.find(work => work.width >= work.height) ?? covers[0]
})
const columns = computed(() => {
  const count = width.value >= 1160 ? 4 : width.value >= 800 ? 3 : width.value >= 315 ? 2 : 1
  const result = Array.from({ length: count }, () => ({ height: 0, items: [] as Work[] }))
  for (const work of works.value) {
    const shortest = result.reduce((shorter, column) => column.height < shorter.height ? column : shorter)
    shortest.items.push(work)
    shortest.height += Math.max(.35, Math.min(2.5, work.height / Math.max(1, work.width))) + .11
  }
  return result.map(column => column.items)
})

let requests: AbortController | undefined
let observer: ResizeObserver | undefined

function media(work: Work, size: 'mobile' | 'desktop' = 'mobile') {
  const variants = work.variants.filter(item => !failed.value.has(item.id))
  const variant = variants.find(item => item.spec_id === size)
    || variants.find(item => item.spec_id === 'mobile')
    || variants.find(item => item.spec_id === 'thumb') || variants[0]
  return variant ? mediaPath(work.album_id, work.id, variant.id) : ''
}
function markFailed(source: string) {
  const match = source.match(/\/variants\/([^/]+)\/body$/)
  if (match?.[1]) failed.value.add(decodeURIComponent(match[1]))
}
function albumLink(id: string) { return { path: albumPath(id), query: { portfolio: '1' } } }
function workLink(work: Work) {
  return {
    path: photoPath(work.album_id, work.id),
    query: { portfolio: '1', from: isWorks.value ? 'works' : 'home', album: filter.value || undefined },
  }
}
function changeCollection(event: Event) {
  const album = (event.target as HTMLSelectElement).value
  void router.replace({ path: '/photos', query: album ? { album } : {} })
}
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
watch(workGrid, node => {
  observer?.disconnect()
  if (node) {
    observer = new ResizeObserver(([entry]) => { width.value = entry?.contentRect.width || 0 })
    observer.observe(node)
  }
}, { flush: 'post' })
watch([isWorks, filter], () => void load())
onMounted(() => void load())
onUnmounted(() => { requests?.abort(); observer?.disconnect() })
</script>

<template>
  <section v-if="!isWorks" class="portfolio-home">
    <header class="portfolio-content portfolio-hero" :class="{ 'has-featured': featured }">
      <div class="portfolio-intro">
        <h1>{{ account.presentation.title }}</h1>
        <p v-if="account.presentation.description" class="preserve">{{ account.presentation.description }}</p>
        <RouterLink v-if="featured" to="/photos" class="portfolio-link">浏览全部作品</RouterLink>
      </div>
      <RouterLink v-if="featured" :to="workLink(featured)" class="portfolio-feature" :aria-label="featured.display_name">
        <img :src="media(featured, 'desktop')" alt="" fetchpriority="high" decoding="async" @error="markFailed(($event.target as HTMLImageElement).src)" />
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
              <div v-for="(work, index) in collection.covers.filter(item => media(item))" :key="work.id"
                class="collection-cover" :class="`cover-${index}`">
                <img :src="media(work)" alt="" loading="lazy" decoding="async" @error="markFailed(($event.target as HTMLImageElement).src)" />
              </div>
              <div v-if="!collection.covers.some(item => media(item))" class="collection-cover cover-0 empty-cover"><ImageIcon :size="27" :stroke-width="1.25" aria-hidden="true" /></div>
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
      <label v-if="collections.length" class="field-label portfolio-filter"><span class="sr-only">筛选相册</span>
        <select :value="filter" @change="changeCollection">
          <option value="">所有相册</option>
          <option v-for="collection in collections" :key="collection.id" :value="collection.id">{{ collection.name }}</option>
        </select>
        <ChevronDown :size="16" aria-hidden="true" />
      </label>
    </div>
    <Notice :error="error" />
    <div v-if="works.length" ref="workGrid" class="portfolio-masonry" :style="{ gridTemplateColumns: `repeat(${columns.length}, minmax(0, 1fr))` }">
      <div v-for="(column, index) in columns" :key="index" class="portfolio-column">
        <RouterLink v-for="work in column" :key="work.id" :to="workLink(work)" class="work-link" :aria-label="work.display_name">
          <div class="work-media" :style="{ aspectRatio: `${Math.max(1, work.width)} / ${Math.max(1, work.height)}` }">
            <img v-if="media(work)" :src="media(work)" alt="" loading="lazy" decoding="async" @error="markFailed(($event.target as HTMLImageElement).src)" />
            <ImageIcon v-else :size="25" :stroke-width="1.25" aria-hidden="true" />
          </div>
        </RouterLink>
      </div>
    </div>
    <div v-if="loading" class="loading" role="status"><SSpinner /> 正在读取作品</div>
    <div v-else-if="!works.length && !error" class="portfolio-empty"><ImageIcon :size="28" :stroke-width="1.2" aria-hidden="true" /><p>暂时还没有公开作品。</p></div>
    <div v-if="cursor || error" class="portfolio-more"><SButton variant="secondary" :loading="loading" @click="load(Boolean(cursor))">{{ error ? '重试' : '加载更多' }}</SButton></div>
  </section>
</template>
