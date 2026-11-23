<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute } from 'vue-router'
import { SButton, SProgressBar, SSpinner } from '@syntrofic/ui'
import { ListTodo, RefreshCw } from 'lucide-vue-next'
import { account, errorText, pathSegment, request, type Job, type Page } from '../api'
import Notice from '../components/Notice.vue'
import Status from '../components/Status.vue'
const route = useRoute()
const id = computed(() => typeof route.params.id === 'string' ? route.params.id : '')
const jobs = ref<Job[]>([])
const item = ref<Job>()
const filter = ref('')
const busy = ref(false)
const loading = ref(true)
const error = ref('')
const visible = computed(() => jobs.value.filter(job => canShow(job) && (!filter.value || job.state === filter.value)))
const kindNames: Record<string, string> = { generate_variant: '生成图片预览', build_archive: '原图打包', backup: '全量备份',
  backup_replica: '异地备份', restore: '恢复图库', restore_drill: '恢复演练', purge_media: '永久清理', storage_migrate_copy: '迁移媒体',
  reconcile: '存储核对', caption_image: '生成照片描述', scan_plan: '规划扫描', rebuild_search: '重建搜索', convert_image: '转换图片' }
let timer: ReturnType<typeof setInterval> | undefined
let controller: AbortController | undefined
function canShow(job: Job) {
  return account.capabilities?.ai_enabled || !['caption_image', 'scan_plan', 'agent_run'].includes(job.kind)
}
async function load(quiet = false) {
  controller?.abort()
  const current = new AbortController()
  controller = current
  if (!quiet) { loading.value = true; error.value = ''; item.value = undefined }
  try {
    if (id.value) item.value = await request<Job>(`/jobs/${pathSegment(id.value)}`, { signal: current.signal })
    else jobs.value = (await request<Page<Job>>('/jobs', { signal: current.signal })).items
  } catch (failure) { if (!current.signal.aborted && !quiet) error.value = errorText(failure) }
  finally { if (!current.signal.aborted) loading.value = false }
}
async function change(action: 'retry' | 'cancel') {
  busy.value = true; error.value = ''
  try { item.value = await request<Job>(`/jobs/${pathSegment(id.value)}/${action}`, { method: 'POST' }) }
  catch (failure) { error.value = errorText(failure) }
  finally { busy.value = false }
}
watch(id, () => void load())
watch(() => account.event, () => void load(true))
onMounted(() => { void load(); timer = setInterval(() => void load(true), 5000) })
onUnmounted(() => { controller?.abort(); if (timer) clearInterval(timer) })
</script>

<template>
  <section class="stack">
    <div v-if="id" class="breadcrumbs"><RouterLink to="/tasks">任务</RouterLink><span>/</span><span>详情</span></div>
    <div class="title-row"><h1>{{ id ? '任务详情' : '后台任务' }}</h1><SButton variant="secondary" @click="load()"><RefreshCw :size="16" />刷新</SButton></div>
    <Notice :error="error" />
    <div v-if="loading" class="loading" role="status"><SSpinner /> 正在读取任务</div>
    <div v-else-if="item && id && canShow(item)" class="panel stack">
      <div class="title-row"><h2>{{ kindNames[item.kind] || item.kind }}</h2><Status :state="item.state" /></div>
      <p class="page-meta identifier">{{ item.id }}</p>
      <SProgressBar v-if="item.progress.total" :value="item.progress.done" :max="item.progress.total" aria-label="任务进度" />
      <p v-if="item.failure">错误代码 <code>{{ item.failure }}</code></p>
      <div class="actions">
        <SButton v-if="['queued','running','retry_wait'].includes(item.state)" variant="secondary" :loading="busy" @click="change('cancel')">取消任务</SButton>
        <SButton v-if="['failed','canceled'].includes(item.state)" :loading="busy" @click="change('retry')">重试任务</SButton>
      </div>
    </div>
    <p v-else-if="id && !error" class="page-meta">这项任务暂不可查看。</p>
    <template v-else-if="!id && !loading">
      <label class="field-label fit">状态<select v-model="filter"><option value="">所有状态</option><option value="queued">等待执行</option><option value="running">处理中</option><option value="succeeded">已完成</option><option value="failed">失败</option><option value="canceled">已取消</option></select></label>
      <ul v-if="visible.length" class="data-list">
        <li v-for="job in visible" :key="job.id"><RouterLink class="panel row-link" :to="`/tasks/${job.id}`">
          <div><strong>{{ kindNames[job.kind] || job.kind }}</strong><p class="page-meta identifier">{{ job.id }}</p></div><Status :state="job.state" />
        </RouterLink></li>
      </ul>
      <div v-else class="empty"><ListTodo :size="32" :stroke-width="1.4" /><p>没有符合条件的任务。</p></div>
    </template>
  </section>
</template>
