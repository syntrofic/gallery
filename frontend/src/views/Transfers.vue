<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { SButton, SDialog, SProgressBar, SSpinner } from '@syntrofic/ui'
import { ArrowDownToLine, CloudUpload, FolderOpen, RefreshCw, X } from 'lucide-vue-next'
import { account, albumPath, errorText, pathSegment, request, type Album, type Operation, type Page, type UploadEntry } from '../api'
import { batches, cancelBatch, createBatch, retryItem, uploadFile, type LocalItem } from '../upload'
import Notice from '../components/Notice.vue'
import Status from '../components/Status.vue'
const route = useRoute()
const router = useRouter()
const uploading = computed(() => route.path.startsWith('/uploads'))
const id = computed(() => typeof route.params.id === 'string' ? route.params.id : '')
const mode = computed(() => uploading.value ? 'uploads' : 'archives')
const root = computed(() => uploading.value ? '/uploads' : '/downloads')
const local = computed(() => batches[id.value])
const history = ref<Operation[]>([])
const selected = ref<Operation>()
const albums = ref<Album[]>([])
const album = ref('')
const pending = ref<File[]>([])
const error = ref('')
const busy = ref(false)
const loading = ref(true)
const dragging = ref(false)
const confirmCancel = ref(false)
const mobile = window.matchMedia('(max-width: 759px), (pointer: coarse)')
const desktop = ref(!mobile.matches)
const max = computed(() => account.capabilities?.max_upload_items || 50)
const details = computed(() => selected.value?.items || [])
let controller: AbortController | undefined
let timer: ReturnType<typeof setInterval> | undefined
function deviceChanged() { desktop.value = !mobile.matches }
async function load(quiet = false) {
  controller?.abort()
  const current = new AbortController()
  controller = current
  if (!quiet) { loading.value = true; error.value = ''; selected.value = undefined }
  try {
    if (id.value) {
      selected.value = await request<Operation>(`/${mode.value}/${pathSegment(id.value)}`, { signal: current.signal })
    } else {
      history.value = (await request<Page<Operation>>(`/${mode.value}`, { signal: current.signal })).items
      if (uploading.value) {
        albums.value = (await request<Page<Album>>('/albums', { signal: current.signal })).items.filter(item => item.capabilities.upload)
        if (!albums.value.some(item => item.id === album.value)) {
          const proposed = route.query.album
          album.value = albums.value.find(item => item.id === proposed)?.id || albums.value[0]?.id || ''
        }
      }
    }
  } catch (failure) { if (!current.signal.aborted && (!quiet || !selected.value)) error.value = errorText(failure) }
  finally { if (!current.signal.aborted) loading.value = false }
}
function add(files?: FileList | null) {
  if (!files) return
  const total = [...pending.value, ...Array.from(files)]
  pending.value = total.slice(0, max.value)
  error.value = total.length > max.value ? `单批最多上传 ${max.value} 张照片。` : ''
}
function drop(event: DragEvent) { dragging.value = false; add(event.dataTransfer?.files) }
async function start() {
  if (!album.value || !pending.value.length) return
  error.value = ''
  busy.value = true
  try {
    const batch = await createBatch(album.value, pending.value)
    pending.value = []
    await router.push(`/uploads/${batch}`)
  } catch (failure) { error.value = errorText(failure) }
  finally { busy.value = false }
}
async function cancel() {
  busy.value = true
  try { selected.value = await cancelBatch(id.value); confirmCancel.value = false }
  catch (failure) { error.value = errorText(failure) }
  finally { busy.value = false }
}
async function resume(entry: UploadEntry, files: FileList | null) {
  if (!files?.length || !entry.id) return
  const file = files[0]!
  if ((entry.name && file.name !== entry.name) || (entry.size !== undefined && file.size !== entry.size)) {
    error.value = '请重选与此条记录名称和大小相同的文件。'
    return
  }
  const item: LocalItem = { id: entry.id, file, name: file.name, size: file.size, percent: 0, state: 'waiting', error: '' }
  try { await uploadFile(id.value, item); await load(true) }
  catch (failure) { error.value = errorText(failure); await load(true) }
}
function size(value: number) {
  return new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 1, style: 'unit', unit: value > 1_048_576 ? 'megabyte' : 'kilobyte' }).format(value / (value > 1_048_576 ? 1_048_576 : 1024))
}
watch(() => route.path, () => void load())
watch(() => account.event, () => void load(true))
onMounted(() => { void load(); mobile.addEventListener('change', deviceChanged); timer = setInterval(() => void load(true), 4000) })
onUnmounted(() => { controller?.abort(); mobile.removeEventListener('change', deviceChanged); if (timer) clearInterval(timer) })
</script>

<template>
  <section class="stack">
    <div v-if="id" class="breadcrumbs"><RouterLink :to="root">{{ uploading ? '上传' : '下载' }}</RouterLink><span>/</span><span>详情</span></div>
    <div class="title-row">
      <div><h1>{{ uploading ? (id ? '上传详情' : '上传照片') : (id ? '原图压缩包' : '下载记录') }}</h1>
        <p v-if="id" class="page-meta identifier">{{ id }}</p></div>
      <div class="actions">
        <SButton variant="secondary" @click="load()"><RefreshCw :size="15" />刷新</SButton>
        <SButton v-if="uploading && id && selected && ['waiting','running'].includes(selected.state)" variant="danger" @click="confirmCancel = true">取消上传</SButton>
      </div>
    </div>
    <Notice :error="error" />
    <div v-if="uploading && !id" class="form-stack panel upload-panel">
      <label class="field-label">目标相册<select v-model="album" :disabled="!albums.length"><option value="">没有可上传的相册</option><option v-for="item in albums" :key="item.id" :value="item.id">{{ item.name }}</option></select></label>
      <div class="drop-zone" :class="{ dragging }" @dragover.prevent="dragging = true" @dragleave="dragging = false" @drop.prevent="drop">
        <CloudUpload :size="34" :stroke-width="1.4" aria-hidden="true" />
        <strong>拖放照片，或从设备选择</strong>
        <span class="page-meta">单批最多 {{ max }} 张</span>
        <label class="inline-action" :class="{ disabled: !album }">
          选择照片<input class="sr-only" type="file" accept="image/*,.tif,.tiff" multiple :disabled="!album"
            @change="add(($event.target as HTMLInputElement).files); ($event.target as HTMLInputElement).value = ''" />
        </label>
      </div>
      <ul v-if="pending.length" class="compact-list">
        <li v-for="(file, index) in pending" :key="index">
          <div><strong>{{ file.name }}</strong><p class="page-meta">{{ size(file.size) }}</p></div>
          <SButton variant="ghost" :aria-label="`移除 ${file.name}`" @click="pending.splice(index, 1)"><X :size="16" /></SButton>
        </li>
      </ul>
      <SButton class="fit" :disabled="!album || !pending.length" :loading="busy" @click="start">上传 {{ pending.length || '' }} 张照片</SButton>
    </div>
    <template v-if="id && selected">
      <div class="panel title-row">
        <RouterLink v-if="selected.album_id || local?.album" :to="albumPath(selected.album_id || local!.album)" class="subtle-link">查看相册</RouterLink>
        <Status :state="selected.state" />
        <a v-if="!uploading && desktop && selected.state === 'ready'" :href="`/api/v1/archives/${pathSegment(id)}/body`" class="inline-action primary-link"><ArrowDownToLine :size="17" />下载原图 ZIP</a>
      </div>
      <Notice v-if="!uploading && !desktop" message="请在桌面打开此页领取整册 ZIP。手机可进入相册逐图下载原图。" />
      <p v-if="selected.failure_code" class="page-meta">{{ selected.failure_code }}</p>
      <ul v-if="uploading && local" class="data-list">
        <li v-for="item in local.items" :key="item.id" class="panel transfer-row">
          <div><strong>{{ item.name }}</strong><p class="page-meta">{{ size(item.size) }}<span v-if="item.error"> · {{ item.error }}</span></p>
            <SProgressBar v-if="['receiving','validating'].includes(item.state)" :value="item.percent" :max="100" :aria-label="`${item.name} 上传进度`" />
          </div>
          <Status :state="item.state" />
          <SButton v-if="item.state === 'failed'" variant="secondary" @click="retryItem(local, item)">重试</SButton>
        </li>
      </ul>
      <ul v-else-if="uploading" class="data-list">
        <li v-for="(item, index) in details" :key="item.id || item.client_item_id" class="panel transfer-row">
          <div><strong>{{ item.name || `照片 ${index + 1}` }}</strong><p v-if="item.failure_code" class="page-meta">{{ item.failure_code }}</p></div>
          <Status :state="item.state" />
          <label v-if="['waiting','failed'].includes(item.state) && selected.state !== 'canceled'" class="inline-action">
            重选文件<input class="sr-only" type="file" accept="image/*,.tif,.tiff" @change="resume(item, ($event.target as HTMLInputElement).files)" />
          </label>
        </li>
      </ul>
    </template>
    <h2 v-if="uploading && !id">上传记录</h2>
    <div v-if="loading && !selected && !history.length" class="loading" role="status"><SSpinner /> 正在读取记录</div>
    <ul v-else-if="!id && history.length" class="data-list">
      <li v-for="row in history" :key="row.id"><RouterLink class="panel row-link" :to="`${root}/${row.id}`"><strong class="identifier">{{ row.id }}</strong><Status :state="row.state" /></RouterLink></li>
    </ul>
    <div v-else-if="!id && !error && !loading" class="empty compact-empty"><FolderOpen :size="28" :stroke-width="1.5" /><p>还没有{{ uploading ? '上传' : '整册下载' }}记录。</p></div>
    <SDialog v-model:open="confirmCancel" title="取消未完成的上传" dismiss-label="关闭">
      <div class="form-stack"><p>已经入册的照片会留在相册里。</p><div class="actions"><SButton variant="secondary" @click="confirmCancel = false">继续上传</SButton><SButton variant="danger" :loading="busy" @click="cancel">取消剩余上传</SButton></div></div>
    </SDialog>
  </section>
</template>
