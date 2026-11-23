<script setup lang="ts">
import { computed, ref } from 'vue'
import { RouterLink, useRoute, useRouter } from 'vue-router'
import { Aperture } from 'lucide-vue-next'
import { SButton, SInput } from '@syntrofic/ui'
import { account, errorText, loadAccount, request, safeDestination, type Session } from '../api'
import Notice from '../components/Notice.vue'

const route = useRoute()
const router = useRouter()
const setup = computed(() => route.path === '/admin/setup')
const username = ref('')
const password = ref('')
const confirm = ref('')
const token = ref('')
const busy = ref(false)
const error = ref('')
async function submit() {
  error.value = ''
  if (setup.value && password.value !== confirm.value) {
    error.value = '两次输入的密码不一致。'
    return
  }
  busy.value = true
  try {
    if (setup.value) {
      await request('/setup/owner', { method: 'POST', body: {
        bootstrap_token: token.value, username: username.value || undefined, password: password.value,
      } })
    }
    const session = await request<Session>('/session', { method: 'POST', body: {
      username: username.value || 'admin', password: password.value,
    } })
    password.value = ''
    confirm.value = ''
    token.value = ''
    account.session = session
    account.initialized = true
    await loadAccount()
    await router.replace(safeDestination(route.query.next))
  } catch (failure) { error.value = errorText(failure) }
  finally { busy.value = false }
}
</script>

<template>
  <main id="gallery-content" class="auth-layout">
    <div class="auth-mark">
      <Aperture :size="34" :stroke-width="1.4" aria-hidden="true" />
      <span>Gallery</span>
    </div>
    <section class="auth-box">
      <h1>{{ setup ? '设置图库' : '登录图库' }}</h1>
      <p v-if="setup">输入主机上运行 <code>gallery admin bootstrap-token</code> 取得的令牌。</p>
      <form class="form-stack" @submit.prevent="submit">
        <SInput v-if="setup" v-model="token" label="初始化令牌" required autocomplete="off" />
        <SInput v-model="username" label="用户名" placeholder="admin" autocomplete="username" :required="!setup" />
        <SInput v-model="password" type="password" label="密码" :autocomplete="setup ? 'new-password' : 'current-password'" required />
        <SInput v-if="setup" v-model="confirm" type="password" label="确认密码" autocomplete="new-password" required />
        <Notice :error="error" />
        <SButton type="submit" :loading="busy">{{ setup ? '创建管理员' : '登录' }}</SButton>
      </form>
      <RouterLink to="/" class="subtle-link">返回公开画廊</RouterLink>
    </section>
  </main>
</template>
