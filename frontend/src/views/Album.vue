<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { SBadge, SButton, SCheckbox, SDialog, SInput, SSpinner, STextArea } from '@syntrofic/ui'
import { ArrowDown, ArrowDownToLine, ArrowUp, CloudUpload, ImagePlus, Images, Settings2 } from 'lucide-vue-next'
import { account, albumPath, ApiError, commandKey, errorText, photoPath, query, request, type Album, type Operation, type Page, type Photo } from '../api'
import Notice from '../components/Notice.vue'
import PhotoTile from '../components/PhotoTile.vue'
import PublicLightbox from '../components/PublicLightbox.vue'
import PublicPhotoGrid from '../components/PublicPhotoGrid.vue'
import { galleryPhoto } from '../galleryPhoto'

const route = useRoute()
const router = useRouter()
const id = computed(() => String(route.params.albumId))
const base = computed(() => albumPath(id.value))
const publicView = computed(() => route.query.portfolio === '1' || !account.session.actor)
const selection = computed(() => publicView.value ? galleryPhoto(route, id.value) : undefined)
const isManage = computed(() => route.path.endsWith('/manage'))
const owner = computed(() => account.session.actor?.role === 'owner')
const album = ref<Album>()
const images = ref<Photo[]>([])
const cursor = ref<string | null>(null)
const error = ref('')
const message = ref('')
const busy = ref(false)
const loading = ref(true)
const sort = ref(typeof route.query.sort === 'string' ? route.query.sort : 'manual')
const chosen = ref<string[]>([])
const selecting = ref(false)
const targets = ref<Album[]>([])
const target = ref('')
const dialog = ref<'remove' | 'move' | 'delete' | ''>('')
const mediaQuery = window.matchMedia('(min-width: 760px) and (pointer: fine)')
const desktop = ref(mediaQuery.matches)
const name = ref('')
const description = ref('')
const selectedCover = ref('')
const publicAlbum = ref(false)
const archiveEnabled = ref(false)
const claims = ref<{ image_id: string; ok: boolean; body_url: string | null; code: string | null }[]>([])
const grants = ref<{ actor_id: string; browse: boolean; upload: boolean; edit: boolean; view_original: boolean; download: boolean }[]>([])
const users = ref<{ id: string; username: string; role: string }[]>([])
const grantUser = ref('')
let controller: AbortController | undefined
let refreshTimer: ReturnType<typeof setInterval> | undefined
let loadedAlbum = ''

function fillDraft(view: Album) {
  name.value = view.name
  description.value = view.description
  selectedCover.value = view.cover_image_id || ''
  publicAlbum.value = view.visibility === 'public'
  archiveEnabled.value = view.archive_enabled
  grants.value = (view.grants || []).map(item => ({ ...item }))
}
async function load(append = false, background = false) {
  controller?.abort()
  const local = new AbortController()
  controller = local
  if (!append && !background) loading.value = true
  if (!background) error.value = ''
  try {
    const [view, page] = await Promise.all([
      request<Album>(base.value, { signal: local.signal }),
      request<Page<Photo>>(`${base.value}/images${query({ sort: sort.value, limit: 48, cursor: append ? cursor.value : undefined })}`, { signal: local.signal }),
    ])
    if (local.signal.aborted) return
    const changed = loadedAlbum !== view.id
    if (changed) { chosen.value = []; claims.value = []; loadedAlbum = view.id }
    if (changed || (!isManage.value && !background) || !album.value) fillDraft(view)
    if (changed || view.acl_revision !== album.value?.acl_revision) {
      chosen.value = []
      claims.value = []
    }
    album.value = view
    images.value = append ? [...images.value, ...page.items] : page.items
    cursor.value = page.next_cursor
  } catch (failure) {
    if (!local.signal.aborted) {
      if (!background || failure instanceof ApiError && [403, 404].includes(failure.status)) error.value = errorText(failure)
      if (failure instanceof ApiError && [403, 404].includes(failure.status)) {
        images.value = []; album.value = undefined; claims.value = []; chosen.value = []
      }
    }
  } finally {
    if (!local.signal.aborted) loading.value = false
  }
}
async function changeSort() {
  chosen.value = []
  await router.replace({ path: route.path, query: { portfolio: publicView.value ? '1' : undefined, sort: sort.value === 'manual' ? undefined : sort.value } })
  await load()
}
function toggle(id: string) {
  chosen.value = chosen.value.includes(id) ? chosen.value.filter(item => item !== id) : [...chosen.value, id]
}
async function action(work: () => Promise<void>, reload = true) {
  if (busy.value) return
  error.value = ''
  message.value = ''
  busy.value = true
  try { await work(); if (reload) await load(false, true) }
  catch (failure) {
    error.value = errorText(failure)
    if (failure instanceof ApiError && failure.status === 412) await load(false, true)
  } finally { busy.value = false }
}
async function itemAction(kind: string, body: unknown) {
  return request(`${base.value}/items/${kind}`, { method: 'POST', body, revision: album.value!.revision })
}
function shift(index: number, offset: number) {
  const item = images.value[index], other = images.value[index + offset]
  if (!item || !other) return
  void action(async () => {
    await itemAction('reorder', { image_id: item.id, ...(offset < 0 ? { before_id: other.id } : { after_id: other.id }) })
    message.value = `已更新「${item.display_name}」的位置。`
  })
}
async function moveDialog() {
  await action(async () => {
    targets.value = (await request<Page<Album>>('/albums')).items.filter(item => item.id !== id.value && item.capabilities.edit)
    target.value = targets.value[0]?.id || ''
    dialog.value = 'move'
  }, false)
}
function confirmAction() {
  const choice = dialog.value
  if (choice === 'move' && !target.value) return
  void action(async () => {
    if (choice === 'delete') {
      await request(base.value, { method: 'DELETE', revision: album.value!.revision })
      dialog.value = ''
      await router.replace('/albums')
      return
    }
    if (choice === 'move') {
      const destination = await request<Album>(albumPath(target.value))
      await itemAction('move', { image_ids: chosen.value, target_album_id: target.value, target_expected_revision: destination.revision })
    } else {
      await itemAction('remove', { image_ids: chosen.value })
    }
    chosen.value = []
    dialog.value = ''
    message.value = choice === 'move' ? '已移动所选照片。' : '已从此相册移出所选照片。'
  }, choice !== 'delete')
}
function saveDetails() {
  void action(async () => {
    const updated = await request<Album>(base.value, {
      method: 'PATCH', revision: album.value!.revision,
      body: { name: name.value.trim(), description: description.value, cover_image_id: selectedCover.value },
    })
    album.value = updated
    message.value = '相册资料已保存。'
  })
}
function saveAccess(includeGrants = false) {
  void action(async () => {
    const updated = await request<Album>(`${base.value}/access`, {
      method: 'PATCH', revision: album.value!.revision,
      body: { visibility: publicAlbum.value ? 'public' : 'private', archive_enabled: archiveEnabled.value,
        ...(includeGrants ? { grants: grants.value } : {}) },
    })
    album.value = updated
    message.value = '访问设置已保存。'
  })
}
async function loadUsers() {
  if (!owner.value) return
  await action(async () => {
    users.value = (await request<Page<{ id: string; username: string; role: string }>>('/admin/users')).items.filter(user => user.role !== 'owner')
  }, false)
}
function addGrant() {
  if (!grantUser.value || grants.value.some(grant => grant.actor_id === grantUser.value)) return
  grants.value.push({ actor_id: grantUser.value, browse: true, edit: false, upload: false, view_original: false, download: false })
  grantUser.value = ''
}
function prepareDownloads() {
  void action(async () => {
    const selected = chosen.value.slice()
    const limit = account.capabilities?.max_download_selection ?? 50
    if (selected.length > limit) throw new Error(`每次最多申请 ${limit} 张原图。`)
    claims.value = (await request<{ items: typeof claims.value }>(`${base.value}/downloads/selected`, {
      method: 'POST', body: { image_ids: selected }, idempotencyKey: commandKey(),
    })).items
  }, false)
}
function archive() {
  void action(async () => {
    const result = await request<Operation>(`${base.value}/archives`, { method: 'POST', desktop: true, idempotencyKey: commandKey() })
    await router.push(`/downloads/${result.id}`)
  }, false)
}
function setDesktop() { desktop.value = mediaQuery.matches }
watch(id, () => { loadedAlbum = ''; void load() })
watch(isManage, enabled => { if (enabled && album.value) fillDraft(album.value) })
watch(() => account.event, () => { if (!busy.value && !isManage.value && !cursor.value) void load(false, true) })
onMounted(() => {
  void load()
  mediaQuery.addEventListener('change', setDesktop)
  refreshTimer = setInterval(() => {
    if (!busy.value && !isManage.value && !cursor.value && images.value.some(photo => !photo.variants?.length)) void load(false, true)
  }, 3500)
})
onUnmounted(() => { controller?.abort(); mediaQuery.removeEventListener('change', setDesktop); if (refreshTimer) clearInterval(refreshTimer) })
</script>

<template>
  <section class="stack" :class="{ 'public-album': publicView }">
    <div class="breadcrumbs"><RouterLink :to="publicView ? '/' : '/albums'">相册</RouterLink><span>/</span><span>{{ album?.name || '详情' }}</span></div>
    <div v-if="loading && !album" class="loading" role="status"><SSpinner /> 正在加载相册</div>
    <Notice :error="error" :message="message" />
    <SButton v-if="error && !album" variant="secondary" class="fit" @click="load()">重试</SButton>
    <template v-if="album">
      <div class="title-row title-top">
        <div><h1>{{ isManage ? '管理相册' : album.name }}</h1><p v-if="album.description && !isManage" class="page-meta preserve">{{ album.description }}</p></div>
        <div class="actions">
          <RouterLink v-if="isManage" :to="base" class="inline-action">返回相册</RouterLink>
          <template v-else>
            <RouterLink v-if="album.capabilities.edit && !publicView" :to="`${base}/manage`" class="inline-action"><Settings2 :size="17" />管理</RouterLink>
            <RouterLink v-if="album.capabilities.upload && account.session.actor && !publicView" :to="{ path: '/uploads', query: { album: id } }" class="inline-action primary-link"><CloudUpload :size="17" />上传照片</RouterLink>
          </template>
        </div>
      </div>
      <div v-if="isManage && album.capabilities.edit" class="manage-grid">
        <form class="panel form-stack" @submit.prevent="saveDetails">
          <h2>相册资料</h2>
          <SInput v-model="name" label="名称" required />
          <STextArea v-model="description" label="描述" :rows="3" />
          <label class="field-label">封面<select v-model="selectedCover"><option value="">自动选择</option>
            <option v-for="photo in images" :key="photo.id" :value="photo.id">{{ photo.display_name }}</option>
          </select></label>
          <SButton type="submit" class="fit" :loading="busy">保存资料</SButton>
        </form>
        <div v-if="owner" class="panel form-stack">
          <h2>访问权限</h2>
          <label class="checkbox-line"><input v-model="publicAlbum" type="checkbox" />允许访客浏览</label>
          <label class="checkbox-line"><input v-model="archiveEnabled" type="checkbox" />开启桌面原图 ZIP</label>
          <SButton variant="secondary" class="fit" :loading="busy" @click="saveAccess()">保存访问设置</SButton>
          <details class="form-stack" @toggle="loadUsers">
            <summary>管理协作者授权</summary>

            <div class="actions"><label class="field-label"><span class="sr-only">协作者</span><select v-model="grantUser">
              <option value="">选择协作者</option><option v-for="user in users" :key="user.id" :value="user.id">{{ user.username }}</option>
            </select></label><SButton variant="secondary" :disabled="!grantUser" @click="addGrant">加入列表</SButton></div>
            <fieldset v-for="(grant, index) in grants" :key="grant.actor_id" class="grant-box">
              <legend>{{ users.find(user => user.id === grant.actor_id)?.username || grant.actor_id }}</legend>
              <label v-for="field in (['browse','edit','upload','view_original','download'] as const)" :key="field" class="checkbox-line">
                <input v-model="grant[field]" type="checkbox" />{{ { browse: '浏览', edit: '编辑', upload: '上传', view_original: '查看原图', download: '下载' }[field] }}
              </label>
              <SButton variant="ghost" @click="grants.splice(index, 1)">移出列表</SButton>
            </fieldset>
            <SButton variant="secondary" class="fit" :loading="busy" @click="saveAccess(true)">保存授权</SButton>
          </details>
          <SButton variant="danger" class="fit" @click="dialog = 'delete'">删除相册</SButton>
        </div>
      </div>
      <Notice v-else-if="isManage && !album.capabilities.edit" error="当前账号无法编辑此相册。" />
      <div class="album-toolbar">
        <div class="actions">
          <label class="field-label"><span class="sr-only">照片排序</span>
            <select v-model="sort" @change="changeSort">
              <option value="manual">相册顺序</option><option value="created_desc">最新上传</option>
              <option value="created_asc">最早上传</option><option value="name_asc">名称</option>
            </select>
          </label>
          <SButton v-if="account.session.actor && !publicView && (album.capabilities.edit || album.capabilities.download)" variant="secondary" :aria-pressed="selecting"
            @click="selecting = !selecting; chosen = []; claims = []">{{ selecting ? '退出选择' : '选择照片' }}</SButton>
          <SButton v-if="desktop && !publicView && album.capabilities.download && album.archive_enabled && account.session.actor" variant="secondary" :loading="busy" @click="archive">
            <ArrowDownToLine :size="16" aria-hidden="true" />整册 ZIP
          </SButton>
        </div>
      </div>
      <div v-if="selecting" class="selection-bar" aria-live="polite">
        <strong>已选 {{ chosen.length }} 张</strong>
        <div class="actions">
          <SButton v-if="album.capabilities.download" variant="secondary" :disabled="!chosen.length" :loading="busy" @click="prepareDownloads">获取原图下载</SButton>
          <SButton v-if="album.capabilities.edit" variant="secondary" :disabled="!chosen.length" @click="moveDialog">移到相册</SButton>
          <SButton v-if="album.capabilities.edit" variant="ghost" :disabled="!chosen.length" @click="dialog = 'remove'">移出此相册</SButton>
        </div>
      </div>
      <div v-if="claims.length" class="panel stack">
        <h2>原图下载</h2>
        <ul class="compact-list">
          <li v-for="claim in claims" :key="claim.image_id">
            <span>{{ images.find(photo => photo.id === claim.image_id)?.display_name || claim.image_id }}</span>
            <a v-if="claim.ok && claim.body_url?.startsWith('/api/v1/')" :href="claim.body_url" class="inline-action">下载原图</a>
            <SBadge v-else tone="warning">不可下载</SBadge>
          </li>
        </ul>
      </div>
      <PublicPhotoGrid v-if="publicView && images.length" :photos="images" :album-id="id" />
      <div v-else-if="images.length" class="photos-grid">
        <article v-for="(photo, index) in images" :key="photo.id" class="photo-card">
          <button v-if="selecting" type="button" class="image-select" :aria-pressed="chosen.includes(photo.id)" @click="toggle(photo.id)">
            <PhotoTile :photo="photo" :album="id" :label="photo.display_name" :eager="index < 4" />
            <span class="item-name">{{ photo.display_name }}</span>
          </button>
          <RouterLink v-else :to="{ path: photoPath(id, photo.id), query: route.query }">
            <PhotoTile :photo="photo" :album="id" :label="photo.display_name" :eager="index < 4" />
            <span class="item-name">{{ photo.display_name }}</span>
          </RouterLink>
          <div v-if="isManage && album.capabilities.edit && sort === 'manual'" class="sort-controls">
            <SButton variant="ghost" size="sm" :disabled="!index || busy" :aria-label="`上移 ${photo.display_name}`" @click="shift(index, -1)"><ArrowUp :size="15" /></SButton>
            <SButton variant="ghost" size="sm" :disabled="index + 1 >= images.length || busy" :aria-label="`下移 ${photo.display_name}`" @click="shift(index, 1)"><ArrowDown :size="15" /></SButton>
          </div>
        </article>
      </div>
      <div v-else-if="!loading && !error" class="empty">
        <ImagePlus v-if="album.capabilities.upload" :size="38" :stroke-width="1.3" />
        <Images v-else :size="38" :stroke-width="1.3" />
        <h2>相册里还没有照片</h2>
        <RouterLink v-if="album.capabilities.upload && account.session.actor && !publicView" :to="{ path: '/uploads', query: { album: id } }" class="inline-action">上传第一张照片</RouterLink>
      </div>
      <SButton v-if="cursor" variant="secondary" class="fit" @click="load(true)">加载更多</SButton>
    </template>
    <SDialog :open="Boolean(dialog)" @update:open="(value: boolean) => { if (!value) dialog = '' }" :title="dialog === 'delete' ? '删除相册' : dialog === 'move' ? '移动所选照片' : '从此相册移出'" dismiss-label="关闭">
      <div class="form-stack">
        <p v-if="dialog !== 'move'">{{ dialog === 'delete' ? '将相册移出相册列表。原图仍保留在图库。' : `将 ${chosen.length} 张照片移出此相册，原图仍保留在图库。` }}</p>
        <label v-else class="field-label">目标相册<select v-model="target"><option value="">没有可写入的目标相册</option><option v-for="item in targets" :key="item.id" :value="item.id">{{ item.name }}</option></select></label>
        <Notice :error="error" />
        <div class="actions"><SButton variant="secondary" @click="dialog = ''">取消</SButton><SButton :variant="dialog === 'move' ? 'primary' : 'danger'" :disabled="dialog === 'move' && !target" :loading="busy" @click="confirmAction">确认</SButton></div>
      </div>
    </SDialog>
  </section>
  <PublicLightbox v-if="selection" :selection="selection" :photos="images" />
</template>
