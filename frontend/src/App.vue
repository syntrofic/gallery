<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { RouterLink, RouterView, useRoute, useRouter } from 'vue-router'
import { SButton, SSpinner } from '@syntrofic/ui'
import { Aperture, ArrowDownToLine, CloudUpload, Images, LibraryBig, ListTodo, LogIn, LogOut, Menu, Settings, Sparkles, Trash2, X } from 'lucide-vue-next'
import { account, errorText, loadAccount, request } from './api'
import Notice from './components/Notice.vue'

const route = useRoute()
const router = useRouter()
const ready = ref(false)
const error = ref('')
const menuOpen = ref(false)
const actor = computed(() => account.session.actor)
const publicSurface = computed(() => route.meta.portfolio || route.query.portfolio === '1' || (!actor.value && !route.meta.authScreen))
const isOwner = computed(() => actor.value?.role === 'owner')
const passwordChange = computed(() => actor.value?.must_change_password)
const visibleLinks = computed(() => [
  { to: '/albums', label: '相册', icon: Images, visible: true },
  { to: '/library', label: '图库', icon: LibraryBig, visible: isOwner.value },
  { to: '/uploads', label: '上传', icon: CloudUpload, visible: Boolean(actor.value) },
  { to: '/downloads', label: '下载', icon: ArrowDownToLine, visible: Boolean(actor.value) },
  { to: '/tasks', label: '任务', icon: ListTodo, visible: Boolean(actor.value) },
  { to: '/agent', label: '助手', icon: Sparkles, visible: Boolean(actor.value && account.capabilities?.ai_enabled) },
  { to: '/trash', label: '回收站', icon: Trash2, visible: isOwner.value },
  { to: '/settings/general', label: '设置', icon: Settings, visible: Boolean(actor.value) },
].filter(item => item.visible && (!passwordChange.value || item.label === '设置')))
let events: EventSource | undefined
let queued: ReturnType<typeof setTimeout> | undefined

function invalidate() {
  if (queued) clearTimeout(queued)
  queued = setTimeout(() => { account.event++ }, 120)
}
function connect() {
  events?.close()
  if (!actor.value || passwordChange.value) return
  const cursor = account.session.event_cursor
  events = new EventSource(cursor === undefined ? '/api/v1/events' : `/api/v1/events?after=${cursor}`)
  events.addEventListener('resync_required', () => {
    events?.close()
    void loadAccount().then(invalidate).catch(() => undefined)
  })
  for (const name of ['job.finished', 'job.progress', 'album.changed', 'image.published',
    'storage.route_changed', 'agent.message', 'agent.delta', 'agent.turn_changed']) {
    events.addEventListener(name, invalidate)
  }
  events.onmessage = invalidate
}

async function start() {
  error.value = ''
  try {
    await loadAccount()
    ready.value = true
  } catch (failure) { error.value = errorText(failure) }
}
async function logout() {
  error.value = ''
  try {
    await request('/session', { method: 'DELETE' })
    account.session = { actor: null }
    await router.replace('/albums')
  } catch (failure) { error.value = errorText(failure) }
}

watch([ready, () => account.initialized, () => actor.value, () => route.fullPath], () => {
  if (!ready.value) return
  menuOpen.value = false
  document.title = route.path === '/' ? account.presentation.title : `${route.meta.title || '相册'} · ${account.presentation.title}`
  if (!account.initialized && (route.meta.auth || route.meta.owner || route.path === '/login')) void router.replace('/admin/setup')
  else if (passwordChange.value && route.path !== '/settings/account') void router.replace('/settings/account')
  else if (!actor.value && (route.meta.auth || route.meta.owner)) {
    void router.replace({ path: '/login', query: { next: route.fullPath } })
  } else if (route.meta.owner && !isOwner.value) void router.replace('/admin/unavailable')
  else if (route.meta.ai && !account.capabilities?.ai_enabled) void router.replace('/albums')
}, { flush: 'sync' })
watch(() => [actor.value?.id, account.session.event_cursor, passwordChange.value], connect)
onMounted(start)
onUnmounted(() => { events?.close(); if (queued) clearTimeout(queued) })
</script>

<template>
  <a class="skip-link" href="#gallery-content">跳到主要内容</a>
  <div v-if="!ready" class="startup">
    <Aperture :size="35" :stroke-width="1.4" aria-hidden="true" />
    <p v-if="!error" role="status"><SSpinner /> 正在打开图库</p>
    <template v-else>
      <Notice :error="error" />
      <SButton variant="secondary" @click="start">重试</SButton>
    </template>
  </div>
  <RouterView v-else-if="route.meta.authScreen" />
  <div v-else-if="publicSurface" class="public-shell">
    <header class="public-masthead">
      <RouterLink to="/" class="public-wordmark">{{ account.presentation.title }}</RouterLink>
      <nav aria-label="公开画廊" class="public-nav">
        <RouterLink to="/" :class="{ active: route.path === '/' }">相册</RouterLink>
        <RouterLink to="/photos" :class="{ active: route.path === '/photos' }">作品</RouterLink>
        <RouterLink v-if="actor" class="admin-entry" to="/albums">管理</RouterLink>
        <a v-else class="admin-entry" href="/admin">管理</a>
      </nav>
    </header>
    <main id="gallery-content" :class="{ 'public-inner': !route.meta.portfolio }" tabindex="-1">
      <Notice :error="error" />
      <RouterView :key="actor?.id || 'visitor'" />
    </main>
  </div>
  <div v-else class="app-shell">
    <header class="masthead">
      <RouterLink to="/" class="wordmark" aria-label="公开画廊首页"><Aperture :size="26" :stroke-width="1.6" /><span>{{ account.presentation.title }}</span></RouterLink>
      <div class="mast-actions">
        <span v-if="actor" class="account-name" :title="actor.username">{{ actor.username }}</span>
        <SButton v-if="actor" variant="ghost" @click="logout"><LogOut :size="16" aria-hidden="true" /><span>退出</span></SButton>
        <RouterLink v-else :to="{ path: '/login', query: { next: route.fullPath } }" class="header-link"><LogIn :size="16" />登录</RouterLink>
        <SButton class="mobile-menu-toggle" variant="secondary" :aria-expanded="menuOpen" aria-controls="primary-navigation" @click="menuOpen = !menuOpen">
          <X v-if="menuOpen" :size="19" aria-hidden="true" /><Menu v-else :size="19" aria-hidden="true" /><span class="sr-only">导航</span>
        </SButton>
      </div>
    </header>
    <nav id="primary-navigation" class="rail" :class="{ 'rail-open': menuOpen }" aria-label="主要导航">
      <RouterLink v-for="item in visibleLinks" :key="item.to" :to="item.to" class="rail-link"
        :class="{ active: route.path.startsWith(item.to.split('/').slice(0, 2).join('/')) }">
        <component :is="item.icon" :size="19" :stroke-width="1.65" aria-hidden="true" />
        <span>{{ item.label }}</span>
      </RouterLink>
    </nav>
    <main id="gallery-content" class="workspace" tabindex="-1">
      <Notice :error="error" />
      <RouterView v-if="!route.meta.ai || account.capabilities?.ai_enabled" :key="actor?.id || 'visitor'" />
    </main>
  </div>
</template>
