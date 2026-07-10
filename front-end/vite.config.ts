import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

const proxy = {
  '/api': {
    target: 'http://127.0.0.1:13500',
    changeOrigin: true,
    ws: true,
  },
  '/uploads': {
    target: 'http://127.0.0.1:13500',
    changeOrigin: true,
  },
}

export default defineConfig({
  plugins: [vue()],
  server: { proxy },
  preview: { proxy },
})
