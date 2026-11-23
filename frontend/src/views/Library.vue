<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { RouterLink, useRoute } from 'vue-router'
import { SButton, SDialog, SSpinner } from '@syntrofic/ui'
import { Images, Trash2 } from 'lucide-vue-next'
import { albumPath, errorText, pathSegment, query, request, type Album, type Page, type Photo } from '../api'
import Notice from '../components/Notice.vue'
import PhotoTile from '../components/PhotoTile.vue'

const route = useRoute()
const isTrash = computed(() => route.path === '/trash')
const items = ref<Photo[]>([])
const albums = ref<Album[]>([])
const coverData = ref<Record<string, Photo>>({})
const cursor = ref<string | null>(null)
const running = ref(false)
const loading = ref(true)
const error = ref('')
const message = ref('')
const selected = ref<Photo>()
const choice = ref<'add' | 'trash' | 'restore' | 'purge'>('add')
const target = ref('')
const open = ref(false)
const title = computed(() => ({ add: '加入相册', trash: '移入回收站', restore: '恢复照片', purge: '永久删除' }[choice.value]))

async function load(append = false) {
  loading.value = true; error.value = ''
  try {
    const result = await request<Page<Photo>>(isTrash.value ? '/trash' : '/library/images' + query({ unassigned: true, cursor: append ? cursor.value : undefined }))
    items.value = append ? [...items.value, ...result.items] : result.items
    cursor.value = result.next_cursor
    if (!isTrash.value) await Promise.allSettled(result.items.map(async row => {
      coverData.value[row.id] = await request<Photo>(`/library/images/${pathSegment(row.id)}`)
    }))
  } catch (failure) { error.value = errorText(failure) }
  finally { loading.value = false }
}
async function prepare(photo: Photo, action: typeof choice.value) {
  choice.value = action
  selected.value = photo
  if (action === 'add') {
    try {
      albums.value = (await request<Page<Album>>('/albums')).items.filter(album => album.capabilities.edit)
      target.value = albums.value[0]?.id || ''
    } catch (failure) { error.value = errorText(failure); return }
  }
  open.value = true
}
async function apply() {
  if (!selected.value) return
  running.value = true; error.value = ''
  try {
    const id = pathSegment(selected.value.id)
    if (choice.value === 'add') {
      if (!target.value) return
      const album = await request<Album>(albumPath(target.value))
      await request(`${albumPath(album.id)}/items/add`, { method: 'POST', body: { image_ids: [selected.value.id] }, revision: album.revision })
    } else {
      const photo = selected.value.lifecycle_revision ? selected.value : await request<Photo>(`/library/images/${id}`)
      if (!photo.lifecycle_revision) throw new Error('请刷新后再试。')
      await request(`/images/${id}/${choice.value}`, { method: 'POST', body: { expected_lifecycle_revision: photo.lifecycle_revision } })
    }
    open.value = false
    await load()
    message.value = choice.value === 'purge' ? '永久删除已排入后台任务。' : '操作已完成。'
  } catch (failure) { error.value = errorText(failure) }
  finally { running.value = false }
}
watch(isTrash, () => { items.value = []; message.value = ''; void load() })
onMounted(() => load())
</script>
<template>
  <section class="stack">
    <div class="title-row"><div><h1>{{ isTrash ? '回收站' : '未归档图库' }}</h1><p class="page-meta">{{ isTrash ? '管理已移入回收站的照片。' : '查看和整理尚未加入相册的原图。' }}</p></div><SButton variant="secondary" @click="load()">刷新</SButton></div>
    <Notice :error="error" :message="message" />
    <div v-if="loading && !items.length" class="loading" role="status"><SSpinner /> 正在读取图库</div>
    <ul v-else-if="isTrash && items.length" class="data-list">
      <li v-for="photo in items" :key="photo.id" class="panel row-link"><strong>{{ photo.display_name }}</strong>
        <div class="actions"><SButton variant="secondary" @click="prepare(photo, 'restore')">恢复</SButton><SButton variant="danger" @click="prepare(photo, 'purge')">永久删除</SButton></div>
      </li>
    </ul>
    <div v-else-if="items.length" class="photos-grid">
      <article class="photo-card" v-for="photo in items" :key="photo.id">
        <RouterLink :to="`/library/images/${pathSegment(photo.id)}`"><PhotoTile :photo="coverData[photo.id]" :label="photo.display_name" /><span class="item-name">{{ photo.display_name }}</span></RouterLink>
        <div class="sort-controls"><SButton variant="ghost" size="sm" @click="prepare(photo, 'add')">加入相册</SButton><SButton variant="ghost" size="sm" :aria-label="`回收 ${photo.display_name}`" @click="prepare(photo, 'trash')"><Trash2 :size="16" /></SButton></div>
      </article>
    </div>
    <div v-else-if="!loading && !error" class="empty"><Trash2 v-if="isTrash" :size="33" :stroke-width="1.4" /><Images v-else :size="33" :stroke-width="1.4" />
      <h2>{{ isTrash ? '回收站为空' : '没有未归档的照片' }}</h2></div>
    <SButton v-if="cursor" variant="secondary" class="fit" @click="load(true)">加载更多</SButton>
    <SDialog v-model:open="open" :title="title" dismiss-label="关闭">
      <div class="form-stack">
        <p>{{ selected?.display_name }}</p>
        <p v-if="choice === 'purge'">永久删除提交后无法从回收站恢复。</p>
        <label v-if="choice === 'add'" class="field-label">目标相册<select v-model="target"><option value="">没有可写入的相册</option><option v-for="album in albums" :key="album.id" :value="album.id">{{ album.name }}</option></select></label>
        <Notice :error="error" />
        <div class="actions"><SButton variant="secondary" @click="open = false">取消</SButton><SButton :variant="choice === 'purge' ? 'danger' : 'primary'" :disabled="choice === 'add' && !target" :loading="running" @click="apply">{{ title }}</SButton></div>
      </div>
    </SDialog>
  </section>
</template>
