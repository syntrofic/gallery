import { createRouter, createWebHistory } from 'vue-router'
import Albums from './views/Albums.vue'
import Portfolio from './views/Portfolio.vue'
import Album from './views/Album.vue'
import Viewer from './views/Viewer.vue'
import SignIn from './views/SignIn.vue'
import Transfers from './views/Transfers.vue'
import Tasks from './views/Tasks.vue'
import Library from './views/Library.vue'
import Settings from './views/Settings.vue'
import Agent from './views/Agent.vue'
import NotFound from './views/NotFound.vue'

export const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', component: Portfolio, meta: { title: '相册', portfolio: true } },
    { path: '/photos', component: Portfolio, meta: { title: '作品', portfolio: true } },
    { path: '/albums', component: Albums, meta: { title: '相册' } },
    { path: '/albums/:albumId', component: Album, meta: { title: '相册' } },
    { path: '/albums/:albumId/manage', component: Album, meta: { title: '管理相册', auth: true } },
    { path: '/albums/:albumId/images/:imageId', component: Viewer, meta: { title: '查看照片' } },
    { path: '/library', component: Library, meta: { title: '图库', owner: true } },
    { path: '/library/images/:imageId', component: Viewer, meta: { title: '查看照片', owner: true } },
    { path: '/trash', component: Library, meta: { title: '回收站', owner: true } },
    { path: '/uploads/:id?', component: Transfers, meta: { title: '上传', auth: true } },
    { path: '/downloads/:id?', component: Transfers, meta: { title: '下载', auth: true } },
    { path: '/tasks/:id?', component: Tasks, meta: { title: '后台任务', auth: true } },
    { path: '/agent/:id?', component: Agent, meta: { title: '图库助手', auth: true, ai: true } },
    { path: '/settings/:section?', component: Settings, meta: { title: '设置', auth: true } },
    { path: '/login', component: SignIn, meta: { title: '登录', authScreen: true } },
    { path: '/admin/setup', component: SignIn, meta: { title: '首次设置', authScreen: true } },
    { path: '/admin/unavailable', component: NotFound, meta: { title: '无法访问后台' } },
    { path: '/:notFound(.*)*', component: NotFound, meta: { title: '页面不存在' } },
  ],
  scrollBehavior(to, from, saved) {
    if (saved) return saved
    if (to.path !== from.path) return { top: 0 }
  },
})
