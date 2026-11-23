import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

const backend = process.env.GALLERY_DEV_BACKEND || 'http://127.0.0.1:8080'

export default defineConfig({
  plugins: [vue()],
  resolve: {
    dedupe: ['vue'],
    alias: [
      { find: /^@syntrofic\/ui\/style\.css$/, replacement: fileURLToPath(new URL('./vendor/ui/style.css', import.meta.url)) },
      { find: /^@syntrofic\/ui$/, replacement: fileURLToPath(new URL('./vendor/ui/index.js', import.meta.url)) },
    ],
  },
  server: { host: '127.0.0.1', proxy: { '/api': backend, '/health': backend } },
})
