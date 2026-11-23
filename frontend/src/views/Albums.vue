<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue'
import { RouterLink, useRouter } from 'vue-router'
import { SBadge, SButton, SDialog, SInput, SSpinner, STextArea } from '@syntrofic/ui'
import { Images, Lock, Plus, Search } from 'lucide-vue-next'
import { account, commandKey, albumPath, errorText, photoPath, query, request, type Album, type Page, type Photo } from '../api'
import Notice from '../components/Notice.vue'
import PhotoTile from '../components/PhotoTile.vue'

const albums = ref<Album[]>([])
const covers = ref<Record<string, Photo | undefined>>({})
const error = ref('')
const loading = ref(true)
const adding = ref(false)
const creating = ref(false)
const createError = ref('')
const name = ref('')
const description = ref('')
const term = ref('')
interface SearchHit { id: string; display_name: string; album_id: string | null }
const searchResults = ref<Album[] | null>(null)
const photoMatches = ref<SearchHit[]>([])
const searchError = ref('')
let searchAbort: AbortController | undefined
const router = useRouter()
const owner = computed(() => account.session.actor?.role === 'owner')
const visible = computed(() => term.value.trim()
  ? albums.value.filter(album => `${album.name} ${album.description}`.toLocaleLowerCase().includes(term.value.trim().toLocaleLowerCase()))
  : albums.value)
const featured = computed(() => !term.value.trim() && visible.value[0]?.cover_image_id ? visible.value[0] : undefined)

async function load() {
  loading.value = true
  error.value = ''
  try {
    const result = await request<Page<Album>>('/albums')
    albums.value = result.items
    const initial = result.items.filter(album => album.cover_image_id).slice(0, 24)
    await Promise.allSettled(initial.map(async album => {
      covers.value[album.id] = await request<Photo>(photoPath(album.id, album.cover_image_id!))
    }))
  } catch (failure) { error.value = errorText(failure) }
  finally { loading.value = false }
}
async function create() {
  if (!name.value.trim()) return
  creating.value = true
  createError.value = ''
  try {
    const album = await request<Album>('/albums', { method: 'POST',
      body: { name: name.value.trim(), description: description.value.trim() },
      idempotencyKey: commandKey(),
    })
    adding.value = false
    await router.push(albumPath(album.id))
  } catch (failure) { createError.value = errorText(failure) }
  finally { creating.value = false }
}
async function search() {
  searchAbort?.abort()
  searchResults.value = null
  photoMatches.value = []
  searchError.value = ''
  if (!term.value.trim()) return
  const controller = new AbortController()
  searchAbort = controller
  try {
    const result = await request<{ items: { albums: { id: string }[]; images: SearchHit[] } }>(
      '/search' + query({ q: term.value }), { signal: controller.signal },
    )
    searchResults.value = albums.value.filter(album => result.items.albums.some(item => item.id === album.id))
    photoMatches.value = result.items.images.filter(item => item.album_id || owner.value)
  } catch (failure) {
    if (!controller.signal.aborted) searchError.value = errorText(failure)
  }
}
onMounted(load)
onUnmounted(() => searchAbort?.abort())
</script>

<template>
  <section class="stack" aria-labelledby="album-title">
    <div class="title-row">
      <div><h1 id="album-title">相册</h1><p class="page-meta">{{ loading ? '正在读取相册' : `${albums.length} 本可见相册` }}</p></div>
      <SButton v-if="owner" @click="adding = true"><Plus :size="17" aria-hidden="true" />新建相册</SButton>
    </div>
    <form class="search-row" role="search" @submit.prevent="search">
      <SInput v-model="term" type="search" label="搜索相册" hide-label placeholder="搜索相册、照片或描述" @update:model-value="searchResults = null; photoMatches = []; searchError = ''" />
      <SButton variant="secondary" type="submit"><Search :size="17" aria-hidden="true" />搜索</SButton>
    </form>
    <Notice :error="error || searchError" />
    <SButton v-if="error" variant="secondary" class="fit" @click="load">重试</SButton>
    <RouterLink v-if="featured && covers[featured.id]" :to="albumPath(featured.id)" class="featured-album">
      <PhotoTile :photo="covers[featured.id]" :album="featured.id" :label="featured.name" eager />
      <div class="featured-caption">
        <div><strong>{{ featured.name }}</strong><p v-if="featured.description">{{ featured.description }}</p></div>
      </div>
    </RouterLink>
    <div v-if="loading" class="loading" role="status"><SSpinner /> 正在加载</div>
    <div v-else-if="(searchResults || visible).length" class="albums-grid">
      <RouterLink v-for="album in (searchResults || visible)" :key="album.id" :to="albumPath(album.id)" class="album-card">
        <PhotoTile :photo="covers[album.id]" :album="album.id" :label="album.name" />
        <div class="album-card-meta">
          <div><h2>{{ album.name }}</h2><p v-if="album.description">{{ album.description }}</p></div>
          <SBadge v-if="album.visibility === 'private'"><Lock :size="12" aria-hidden="true" />私有</SBadge>
        </div>
      </RouterLink>
    </div>
    <div v-else-if="!error && !photoMatches.length" class="empty">
      <Images :size="38" :stroke-width="1.35" aria-hidden="true" />
      <h2>{{ term ? '没有找到匹配结果' : '这里还没有相册' }}</h2>
      <p v-if="!term">{{ owner ? '新建一本相册，就可以上传第一张照片。' : '登录可查看当前账号有权访问的相册。' }}</p>
      <SButton v-if="owner && !term" variant="secondary" @click="adding = true">新建相册</SButton>
    </div>
    <div v-if="photoMatches.length" class="stack">
      <h2>匹配的照片</h2>
      <ul class="data-list"><li v-for="item in photoMatches" :key="item.id">
        <RouterLink class="panel row-link" :to="item.album_id ? photoPath(item.album_id, item.id) : `/library/images/${encodeURIComponent(item.id)}`">
          <strong>{{ item.display_name }}</strong><span class="page-meta">{{ albums.find(album => album.id === item.album_id)?.name || '未归档' }}</span>
        </RouterLink>
      </li></ul>
    </div>
    <SDialog v-model:open="adding" title="新建相册" dismiss-label="关闭">
      <form class="form-stack" @submit.prevent="create">
        <SInput v-model="name" label="名称" required />
        <STextArea v-model="description" label="描述" :rows="3" />
        <Notice :error="createError" />
        <div class="actions"><SButton variant="secondary" @click="adding = false">取消</SButton><SButton type="submit" :loading="creating">创建相册</SButton></div>
      </form>
    </SDialog>
  </section>
</template>
