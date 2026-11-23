<script setup lang="ts">
import { SButton } from '@syntrofic/ui'
import { FolderOpen } from 'lucide-vue-next'
</script>
<template>
  <section class="empty">
    <FolderOpen :size="32" :stroke-width="1.4" aria-hidden="true" />
    <h1>无法打开这个页面</h1>
    <p>页面可能已移动，或当前账号没有访问权限。</p>
    <SButton href="/albums" variant="secondary">返回相册</SButton>
  </section>
</template>
