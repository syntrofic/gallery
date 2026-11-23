<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { SAlert, SButton, SSpinner, STextArea } from '@syntrofic/ui'
import { Plus, Sparkles } from 'lucide-vue-next'
import { account, commandKey, errorText, pathSegment, request, type Album, type Page } from '../api'
import Notice from '../components/Notice.vue'
import Status from '../components/Status.vue'
interface Conversation { id: string; state: string; turn_id?: string | null; turn_state?: string | null; scope?: { albums?: string[] }; revision?: number;
  actions?: { id: string; state: string; tool_name: string; revision?: number }[]; running_jobs?: { id: string; state: string; kind: string }[] }
interface Message { id: string; seq: number; client_message_id: string; delivery_state: string; text: string }
const route = useRoute()
const router = useRouter()
const id = computed(() => typeof route.params.id === 'string' ? route.params.id : '')
const history = ref<Conversation[]>([])
const conversation = ref<Conversation>()
const messages = ref<Message[]>([])
const albums = ref<Album[]>([])
const selected = ref('')
const draft = ref('')
const error = ref('')
const running = ref(false)
const loading = ref(true)
const active = computed(() => ['starting','running','stop_requested'].includes(conversation.value?.turn_state || ''))
const endpoint = computed(() => `/agent/sessions/${pathSegment(id.value)}`)
const waiting = computed(() => messages.value.some(msg => msg.delivery_state === 'queued'))
let timer: ReturnType<typeof setInterval> | undefined
let controller: AbortController | undefined
async function load(quiet = false) {
  if (!account.capabilities?.ai_enabled) { loading.value = false; return }
  controller?.abort()
  const current = new AbortController()
  controller = current
  if (!quiet) { loading.value = true; error.value = ''; conversation.value = undefined; messages.value = [] }
  try {
    if (id.value) {
      const [view, thread] = await Promise.all([
        request<Conversation>(endpoint.value, { signal: current.signal }),
        request<Page<Message>>(`${endpoint.value}/messages`, { signal: current.signal }),
      ])
      if (!current.signal.aborted) { conversation.value = view; messages.value = thread.items }
    } else {
      const [list, available] = await Promise.all([request<Page<Conversation>>('/agent/sessions'), request<Page<Album>>('/albums')])
      if (!current.signal.aborted) {
        history.value = list.items
        albums.value = available.items.filter(album => album.capabilities.edit)
      }
    }
  } catch (failure) { if (!current.signal.aborted && !quiet) error.value = errorText(failure) }
  finally { if (!current.signal.aborted) loading.value = false }
}
async function action(work: () => Promise<void>) {
  if (running.value) return
  running.value = true; error.value = ''
  try { await work(); await load(true) } catch (failure) { error.value = errorText(failure) }
  finally { running.value = false }
}
function create() {
  void action(async () => {
    if (!selected.value) return
    const session = await request<Conversation>('/agent/sessions', {
      method: 'POST', body: { scope: { albums: [selected.value] } }, idempotencyKey: commandKey(),
    })
    await router.push(`/agent/${session.id}`)
  })
}
async function dispatch() {
  await request(`${endpoint.value}/dispatch`, { method: 'POST' })
}
function send() {
  if (!draft.value.trim()) return
  void action(async () => {
    await request(`${endpoint.value}/messages`, {
      method: 'POST', body: { client_message_id: commandKey(), text: draft.value, mode: 'queue' },
    })
    draft.value = ''
    if (!active.value) await dispatch()
  })
}
function stop() {
  const turn = conversation.value?.turn_id
  if (!turn) return
  void action(async () => { await request(`${endpoint.value}/turns/${pathSegment(turn)}/stop`, { method: 'POST', idempotencyKey: commandKey() }) })
}
function decide(id: string, revision: number | undefined, approve: boolean) {
  if (revision === undefined) return
  void action(async () => { await request(`${endpoint.value}/actions/${pathSegment(id)}/${approve ? 'approve' : 'reject'}`, {
    method: 'POST', body: { expected_revision: revision }, idempotencyKey: commandKey(),
  }) })
}
watch(id, () => { draft.value = ''; void load() })
watch(() => account.event, () => { if (id.value) void load(true) })
onMounted(() => { void load(); timer = setInterval(() => { if (id.value) void load(true) }, 2000) })
onUnmounted(() => { controller?.abort(); if (timer) clearInterval(timer) })
</script>
<template>
  <section class="stack">
    <div v-if="id" class="breadcrumbs"><RouterLink to="/agent">助手</RouterLink><span>/</span><span>会话</span></div>
    <div class="title-row"><h1>图库助手</h1><Status v-if="conversation?.turn_state" :state="conversation.turn_state" /></div>
    <Notice :error="error" />
    <SAlert v-if="!account.capabilities?.ai_enabled" title="助手未启用">管理员可以在服务端启用并配置模型。</SAlert>
    <template v-else-if="!id">
      <form class="panel form-stack narrow" @submit.prevent="create">
        <h2>新会话</h2>
        <label class="field-label">本次操作范围<select v-model="selected" required><option value="">选择一个相册</option><option v-for="album in albums" :key="album.id" :value="album.id">{{ album.name }}</option></select></label>
        <SButton type="submit" class="fit" :disabled="!selected" :loading="running"><Plus :size="16" />开始会话</SButton>
      </form>
      <div v-if="history.length" class="stack"><h2>最近会话</h2><ul class="data-list"><li v-for="thread in history" :key="thread.id"><RouterLink class="panel row-link" :to="`/agent/${thread.id}`"><span class="identifier">{{ thread.id }}</span><Status :state="thread.state" /></RouterLink></li></ul></div>
    </template>
    <div v-else-if="loading" class="loading" role="status"><SSpinner /> 正在读取会话</div>
    <div v-else-if="conversation" class="agent-grid">
      <div class="panel stack">
        <div v-if="!messages.length" class="empty compact-empty"><Sparkles :size="28" :stroke-width="1.4" /><p>可以让助手检查本相册的照片或整理描述。</p></div>
        <ol v-else class="conversation" aria-label="会话消息">
          <li v-for="msg in messages.filter(item => item.delivery_state !== 'removed')" :key="msg.id" :class="{ reply: msg.client_message_id.startsWith('assistant:') }">
            <div class="title-row"><strong>{{ msg.client_message_id.startsWith('assistant:') ? '助手' : '我' }}</strong><Status v-if="msg.delivery_state !== 'accepted'" :state="msg.delivery_state" /></div><p class="preserve">{{ msg.text }}</p>
          </li>
        </ol>
        <form class="form-stack" @submit.prevent="send">
          <STextArea v-model="draft" label="消息" :rows="3" required />
          <div class="actions">
            <SButton type="submit" :disabled="!draft.trim()" :loading="running">{{ active ? '加入队列' : '发送' }}</SButton>
            <SButton v-if="active" variant="danger" @click="stop">停止当前轮</SButton>
            <SButton v-else-if="waiting" variant="secondary" @click="action(dispatch)">执行等待中的消息</SButton>
          </div>
          <p v-if="active" class="page-meta">停止对话后，已提交的后台任务会继续执行。</p>
        </form>
      </div>
      <aside class="panel stack"><h2>本次操作</h2>
        <p v-if="!conversation.actions?.length && !conversation.running_jobs?.length" class="page-meta">暂无工具记录或运行中的任务。</p>
        <ul v-if="conversation.actions?.length" class="compact-list">
          <li class="stack" v-for="entry in conversation.actions" :key="entry.id">
            <div class="title-row"><code>{{ entry.tool_name }}</code><Status :state="entry.state" /></div>
            <div v-if="entry.state === 'proposed'" class="actions"><SButton variant="secondary" @click="decide(entry.id, entry.revision, false)">拒绝</SButton><SButton @click="decide(entry.id, entry.revision, true)">确认</SButton></div>
          </li>
        </ul>
        <ul v-if="conversation.running_jobs?.length" class="compact-list"><li v-for="job in conversation.running_jobs" :key="job.id"><RouterLink :to="`/tasks/${job.id}`" class="subtle-link">{{ job.kind }}</RouterLink><Status :state="job.state" /></li></ul>
      </aside>
    </div>
  </section>
</template>
