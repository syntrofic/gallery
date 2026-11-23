<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { SBadge, SButton, SDialog, SInput, SSpinner, STextArea } from '@syntrofic/ui'
import { account, commandKey, errorText, pathSegment, request, type Page, type Presentation } from '../api'
import Notice from '../components/Notice.vue'
import Status from '../components/Status.vue'

interface Settings {
  revision: number; storage: { kind: string }; ai: { enabled: boolean };
  limits: Record<'max_upload_items' | 'upload_concurrency_hint' | 'max_download_selection' | 'ai_daily_token_budget', number>
}
interface User { id: string; username: string; role: string; active: boolean; revision: number }
interface Backup { id: string; state: string; phase?: string; snapshot_at?: number; replica_verified_at?: number; verified_at?: number; job_id?: string }
interface Restore { id: string; job_id?: string; state?: string; snapshot_at?: number }
interface Profile { revision: number; specs: { spec_id: string; fit: string; format: string; png_compression: string; long_edge: number | null; width: number | null; height: number | null }[] }
interface Migration { id: string; stage: string; state: string; revision: number; preview_sha?: string; blockers?: string[] }
const route = useRoute()
const router = useRouter()
const owner = computed(() => account.session.actor?.role === 'owner')
const forced = computed(() => account.session.actor?.must_change_password)
const page = computed(() => forced.value ? 'account' : String(route.params.section || (owner.value ? 'general' : 'account')))
const pages = [{ id: 'general', label: '概览' }, { id: 'account', label: '密码' }, { id: 'limits', label: '使用限额' }, { id: 'images', label: '图片规格' }, { id: 'storage', label: '存储' }, { id: 'users', label: '协作者' }, { id: 'backups', label: '备份与恢复' }, { id: 'audit', label: '操作记录' }]
const settings = ref<Settings>()
const presentation = ref<Presentation>({ ...account.presentation })
const profile = ref<Profile>()
const users = ref<User[]>([])
const backups = ref<Backup[]>([])
const alerts = ref<string[]>([])
const audit = ref<{ id: string; type?: string; kind?: string; occurred_at?: string }[]>([])
const migration = ref<Migration>()
const probe = ref<Record<string, unknown>>()
const restore = ref<Restore>()
const restorePlan = ref<{ unbacked_images?: number; loss_sha?: string; snapshot_at?: number; gap_ms?: number }>()
const restoreId = ref('')
const restoreOpen = ref(false)
const lossConfirmed = ref(false)
const verifyReport = ref<string>('')
const adminAction = ref<'create' | 'reset' | ''>('')
const adminUser = ref<User>()
const current = ref('')
const password = ref('')
const repeat = ref('')
const username = ref('')
const collaboratorPassword = ref('')
const busy = ref(false)
const loading = ref(false)
const error = ref('')
const message = ref('')
const allLimitFields = [
  { key: 'max_upload_items', label: '每批上传上限' },
  { key: 'upload_concurrency_hint', label: '并行上传数' },
  { key: 'max_download_selection', label: '原图领取上限' },
  { key: 'ai_daily_token_budget', label: '每日 AI Token 预算' },
] as const
const limitFields = computed(() => allLimitFields.filter(field => field.key !== 'ai_daily_token_budget' || settings.value?.ai.enabled))
const tmpLimits = ref<Record<string, number>>({})
const backendKind = ref<'local' | 's3' | 'web_dav'>('local')
const backend = ref({ root: '', endpoint: '', bucket: '', region: 'us-east-1', prefix: '', access_key: '',
  secret_key: '', origin: '', collection: '', username: '', password: '' })
function date(value?: number) { return value ? new Intl.DateTimeFormat('zh-CN', { dateStyle: 'medium', timeStyle: 'short' }).format(new Date(value)) : '—' }

async function load() {
  error.value = ''
  if (!owner.value || ['account'].includes(page.value)) return
  loading.value = true
  try {
    if (['general','limits','storage'].includes(page.value)) {
      settings.value = await request<Settings>('/admin/settings')
      if (page.value === 'general') presentation.value = { ...account.presentation }
      tmpLimits.value = Object.fromEntries(limitFields.value.map(field => [field.key, settings.value!.limits[field.key]]))
    } else if (page.value === 'users') users.value = (await request<Page<User>>('/admin/users')).items
    else if (page.value === 'images') profile.value = await request<Profile>('/variant-profiles/current')
    else if (page.value === 'audit') audit.value = (await request<Page<typeof audit.value[number]>>('/admin/audit')).items
    else if (page.value === 'backups') {
      const data = await request<Page<Backup> & { health?: { alerts?: string[] } }>('/admin/backups')
      backups.value = data.items; alerts.value = data.health?.alerts || []
    }
  } catch (failure) { error.value = errorText(failure) }
  finally { loading.value = false }
}
async function action(work: () => Promise<void>) {
  if (busy.value) return
  busy.value = true; error.value = ''; message.value = ''
  try { await work() } catch (failure) { error.value = errorText(failure) }
  finally { busy.value = false }
}
function changePassword() {
  if (password.value !== repeat.value) { error.value = '两次输入的密码不同。'; return }
  void action(async () => {
    await request('/account/password', { method: 'PATCH', body: { current_password: current.value, new_password: password.value, confirm_password: repeat.value } })
    current.value = ''; password.value = ''; repeat.value = ''
    account.session = { actor: null }
    await router.replace('/login')
  })
}
function savePresentation() {
  void action(async () => {
    const result = await request<Presentation>('/admin/settings/presentation', {
      method: 'PATCH', body: { title: presentation.value.title, description: presentation.value.description,
        expected_revision: presentation.value.revision },
    })
    account.presentation = result
    presentation.value = { ...result }
    message.value = '公开画廊资料已保存。'
  })
}
function saveLimits() {
  void action(async () => {
    await request('/admin/settings/limits', { method: 'PATCH', body: tmpLimits.value, revision: settings.value?.revision })
    await load(); message.value = '限额已更新。'
  })
}
function saveProfile() {
  if (!profile.value) return
  void action(async () => {
    await request('/variant-profiles/current', { method: 'PUT', body: profile.value!.specs, revision: profile.value!.revision })
    await load(); message.value = '图片规格已更新。重建进度可在任务中查看。'
  })
}
function userDialog(user?: User) {
  adminUser.value = user; username.value = ''; collaboratorPassword.value = ''
  adminAction.value = user ? 'reset' : 'create'
}
function saveUser() {
  void action(async () => {
    if (adminUser.value) await request(`/admin/users/${pathSegment(adminUser.value.id)}/reset-password`, {
      method: 'POST', revision: adminUser.value.revision, body: { password: collaboratorPassword.value },
    })
    else await request('/admin/users', { method: 'POST', body: { username: username.value, password: collaboratorPassword.value } })
    adminAction.value = ''; collaboratorPassword.value = ''
    await load(); message.value = '协作者已更新。'
  })
}
function toggleUser(user: User) {
  void action(async () => {
    await request(`/admin/users/${pathSegment(user.id)}`, { method: 'PATCH', revision: user.revision, body: { status: user.active ? 'disabled' : 'active' } })
    await load(); message.value = '协作者已更新。'
  })
}
function startBackup() {
  void action(async () => {
    await request('/admin/backups', { method: 'POST', idempotencyKey: commandKey() })
    await load(); message.value = '备份已加入任务队列。'
  })
}
function verify(row: Backup) {
  void action(async () => {
    const result = await request<{ ok: boolean; objects_verified: number }>(`/admin/backups/${pathSegment(row.id)}/verify`, { method: 'POST' })
    verifyReport.value = result.ok ? `已验证 ${result.objects_verified} 张原图。` : '备份完整性检查失败。'
    await load()
  })
}
function preview(row: Backup) {
  void action(async () => {
    restoreId.value = row.id; lossConfirmed.value = false
    restorePlan.value = await request('/admin/restores', { method: 'POST', body: { backup_id: row.id, mode: 'preview' } })
    restoreOpen.value = true
  })
}
function executeRestore() {
  if (restorePlan.value?.unbacked_images && !lossConfirmed.value) return
  void action(async () => {
    restore.value = await request<Restore>('/admin/restores', { method: 'POST', body: {
      backup_id: restoreId.value, mode: 'execute', confirm_loss: lossConfirmed.value ? restorePlan.value?.loss_sha : undefined,
    } })
    restoreOpen.value = false
    message.value = '隔离恢复已加入队列。'
  })
}
function refreshRestore() {
  void action(async () => { restore.value = await request<Restore>(`/admin/restores/${pathSegment(restore.value!.id)}`) })
}
function planStorage() {
  void action(async () => {
    const kind = backendKind.value
    const { root, endpoint, region, bucket, prefix, access_key, secret_key, origin, collection, username, password } = backend.value
    const details = kind === 'local' ? { root } : kind === 's3'
      ? { endpoint: endpoint || undefined, region, bucket, prefix, access_key: access_key || undefined, secret_key: secret_key || undefined }
      : { origin, collection, username: username || undefined, password: password || undefined }
    migration.value = await request<Migration>('/admin/storage-migrations', { method: 'POST', body: { kind, ...details }, idempotencyKey: commandKey() })
    backend.value.access_key = ''; backend.value.secret_key = ''; backend.value.password = ''
  })
}
function migrationAction(command?: string) {
  if (!migration.value) return
  void action(async () => {
    const value = migration.value!
    const base = `/admin/storage-migrations/${pathSegment(value.id)}`
    migration.value = command
      ? await request<Migration>(`${base}/${command}`, { method: 'POST', body: ['start','cutover'].includes(command) ? { expected_revision: value.revision } : undefined })
      : await request<Migration>(base)
  })
}
watch(page, () => { message.value = ''; void load() })
onMounted(load)
</script>

<template>
  <section class="stack">
    <div class="title-row"><h1>设置</h1><SButton v-if="owner && page !== 'account'" variant="secondary" @click="load">刷新</SButton></div>
    <nav v-if="owner && !forced" class="tabs" aria-label="设置分区">
      <RouterLink v-for="item in pages" :key="item.id" :to="`/settings/${item.id}`" :class="{ active: page === item.id }">{{ item.label }}</RouterLink>
    </nav>
    <Notice :error="error" :message="message" />
    <div v-if="loading" class="loading" role="status"><SSpinner /> 正在读取设置</div>
    <form v-if="page === 'account' || !owner" class="panel form-stack narrow" @submit.prevent="changePassword">
      <h2>{{ forced ? '设置新密码' : '修改密码' }}</h2>
      <SInput v-model="current" type="password" label="当前密码" autocomplete="current-password" required />
      <SInput v-model="password" type="password" label="新密码" autocomplete="new-password" required />
      <SInput v-model="repeat" type="password" label="确认新密码" autocomplete="new-password" required />
      <SButton type="submit" :loading="busy" class="fit">保存密码</SButton>
    </form>
    <div v-else-if="page === 'general' && settings" class="manage-grid">
      <form class="panel form-stack presentation-settings" @submit.prevent="savePresentation">
        <div class="title-row"><h2>公开画廊</h2><RouterLink to="/" class="subtle-link">查看公开首页</RouterLink></div>
        <SInput v-model="presentation.title" label="画廊名称" required />
        <STextArea v-model="presentation.description" label="简介" :rows="3" />
        <SButton type="submit" :loading="busy" class="fit">保存公开资料</SButton>
      </form>
      <section class="panel stack"><h2>存储</h2><p>{{ settings.storage.kind }}</p><RouterLink to="/settings/storage" class="subtle-link">查看存储配置</RouterLink></section>
      <section v-if="settings.ai.enabled" class="panel stack"><h2>图库助手</h2><SBadge tone="success" class="fit">已启用</SBadge><RouterLink to="/agent" class="subtle-link">打开图库助手</RouterLink></section>
      <section class="panel stack"><h2>备份</h2><RouterLink to="/settings/backups" class="subtle-link">查看快照、演练和异地副本</RouterLink></section>
    </div>
    <form v-else-if="page === 'limits' && settings" class="panel form-stack narrow" @submit.prevent="saveLimits">
      <h2>使用限额</h2>
      <label v-for="field in limitFields" :key="field.key" class="field-label">{{ field.label }}<input v-model.number="tmpLimits[field.key]" type="number" min="0" required /></label>
      <SButton type="submit" :loading="busy" class="fit">保存限额</SButton>
    </form>
    <form v-else-if="page === 'images' && profile" class="panel stack" @submit.prevent="saveProfile">
      <div class="title-row"><h2>衍生图规格</h2><SBadge>修订 {{ profile.revision }}</SBadge></div>
      <fieldset v-for="spec in profile.specs" :key="spec.spec_id" class="field-grid">
        <legend>{{ spec.spec_id }}</legend>
        <label class="field-label">格式<select v-model="spec.format"><option>png</option><option>jpeg</option><option>webp</option></select></label>
        <label v-if="spec.fit !== 'cover'" class="field-label">最长边<input v-model.number="spec.long_edge" type="number" min="1" /></label>
        <template v-else><label class="field-label">宽度<input v-model.number="spec.width" type="number" min="1" /></label><label class="field-label">高度<input v-model.number="spec.height" type="number" min="1" /></label></template>
      </fieldset>
      <SButton type="submit" :loading="busy" class="fit">保存并重建衍生图</SButton>
    </form>
    <div v-else-if="page === 'users'" class="stack">
      <div class="title-row"><h2>协作者</h2><SButton @click="userDialog()">添加协作者</SButton></div>
      <ul class="data-list"><li v-for="user in users" :key="user.id" class="panel row-link">
        <div><strong>{{ user.username }}</strong><p class="page-meta">{{ user.role === 'owner' ? '所有者' : user.active ? '已启用' : '已停用' }}</p></div>
        <div v-if="user.role !== 'owner'" class="actions"><SButton variant="secondary" @click="userDialog(user)">重置密码</SButton><SButton variant="ghost" @click="toggleUser(user)">{{ user.active ? '停用' : '启用' }}</SButton></div>
      </li></ul>
    </div>
    <div v-else-if="page === 'backups'" class="stack">
      <div class="title-row"><h2>备份快照</h2><SButton :loading="busy" @click="startBackup">运行备份</SButton></div>
      <Notice v-if="alerts.length" :message="alerts.join(' · ')" />
      <p v-if="verifyReport" role="status">{{ verifyReport }}</p>
      <div v-if="restore" class="panel stack"><h2>隔离恢复</h2><p class="identifier">{{ restore.id }}</p><Status v-if="restore.state" :state="restore.state" />
        <div class="actions"><RouterLink v-if="restore.job_id" :to="`/tasks/${restore.job_id}`" class="subtle-link">查看任务</RouterLink><SButton variant="secondary" @click="refreshRestore">刷新恢复状态</SButton></div></div>
      <ul v-if="backups.length" class="data-list"><li v-for="snapshot in backups" :key="snapshot.id" class="panel row-link">
        <div><strong>{{ date(snapshot.snapshot_at) }}</strong><p class="page-meta identifier">{{ snapshot.id }}</p><p v-if="snapshot.replica_verified_at" class="page-meta">异地副本已核验</p></div>
        <div class="actions"><Status :state="snapshot.phase || snapshot.state" /><template v-if="snapshot.state === 'succeeded'">
          <SButton variant="secondary" :loading="busy" @click="verify(snapshot)">核验</SButton><SButton variant="ghost" @click="preview(snapshot)">恢复预检</SButton>
        </template></div>
      </li></ul>
      <div v-else-if="!loading" class="empty compact-empty">还没有备份快照。</div>
    </div>
    <div v-else-if="page === 'storage'" class="stack">
      <div class="panel title-row"><div><h2>活动存储</h2><p class="page-meta">{{ settings?.storage.kind }}</p></div><SButton variant="secondary" :loading="busy" @click="action(async () => { probe = await request('/admin/storage/probe', { method: 'POST' }) })">检查连接</SButton></div>
      <pre v-if="probe" class="panel result">{{ JSON.stringify(probe, null, 2) }}</pre>
      <form class="panel form-stack" @submit.prevent="planStorage">
        <h2>迁移到新存储</h2>
        <label class="field-label fit">目标类型<select v-model="backendKind"><option value="local">本地文件系统</option><option value="s3">S3 兼容存储</option><option value="web_dav">WebDAV</option></select></label>
        <SInput v-if="backendKind === 'local'" v-model="backend.root" label="目标绝对路径" required />
        <div v-else-if="backendKind === 's3'" class="field-grid"><SInput v-model="backend.endpoint" label="服务地址" /><SInput v-model="backend.bucket" label="桶名称" required /><SInput v-model="backend.region" label="区域" required /><SInput v-model="backend.prefix" label="前缀" /><SInput v-model="backend.access_key" label="访问密钥 ID" autocomplete="off" /><SInput v-model="backend.secret_key" type="password" label="访问密钥" autocomplete="new-password" /></div>
        <div v-else class="field-grid"><SInput v-model="backend.origin" label="服务器地址" required /><SInput v-model="backend.collection" label="目录" required /><SInput v-model="backend.username" label="用户名" /><SInput v-model="backend.password" type="password" label="密码" autocomplete="new-password" /></div>
        <SButton type="submit" variant="secondary" :loading="busy" class="fit">预检迁移</SButton>
      </form>
      <div v-if="migration" class="panel stack"><div class="title-row"><h2>迁移状态</h2><Status :state="migration.state" /></div><p class="identifier">{{ migration.id }}</p>
        <div class="actions"><SButton variant="secondary" @click="migrationAction()">刷新</SButton>
          <SButton v-if="migration.stage === 'planned'" @click="migrationAction('start')">开始复制</SButton>
          <SButton v-if="migration.stage === 'copying'" variant="secondary" @click="migrationAction('pause')">暂停</SButton>
          <SButton v-if="migration.state === 'paused'" variant="secondary" @click="migrationAction('resume')">继续</SButton>
          <SButton v-if="migration.stage === 'ready'" @click="migrationAction('cutover')">切换活动存储</SButton>
        </div></div>
    </div>
    <div v-else-if="page === 'audit'" class="stack"><h2>操作记录</h2>
      <ul v-if="audit.length" class="data-list"><li v-for="row in audit" :key="row.id" class="panel row-link"><strong>{{ row.type || row.kind }}</strong><span class="page-meta">{{ row.occurred_at }}</span></li></ul><p v-else-if="!loading">暂无操作记录。</p>
    </div>
    <SDialog :open="Boolean(adminAction)" @update:open="(value: boolean) => { if (!value) adminAction = '' }" :title="adminAction === 'reset' ? '重置协作者密码' : '添加协作者'" dismiss-label="关闭">
      <form class="form-stack" @submit.prevent="saveUser"><SInput v-if="!adminUser" v-model="username" label="用户名" required /><p v-else>{{ adminUser.username }}</p>
        <SInput v-model="collaboratorPassword" type="password" label="临时密码" autocomplete="new-password" required /><Notice :error="error" /><SButton type="submit" :loading="busy">保存</SButton>
      </form>
    </SDialog>
    <SDialog v-model:open="restoreOpen" title="从备份隔离恢复" dismiss-label="关闭">
      <div class="form-stack"><p>快照时间 {{ date(restorePlan?.snapshot_at) }}</p><p>未进入此备份的照片 {{ restorePlan?.unbacked_images || 0 }} 张</p>
        <label v-if="restorePlan?.unbacked_images" class="checkbox-line"><input v-model="lossConfirmed" type="checkbox" />确认恢复到此时间点</label>
        <Notice :error="error" /><SButton variant="danger" :disabled="Boolean(restorePlan?.unbacked_images) && !lossConfirmed" :loading="busy" @click="executeRestore">开始隔离恢复</SButton></div>
    </SDialog>
  </section>
</template>
