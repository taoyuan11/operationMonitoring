import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

const proxy = {
  '/api': {
    target: 'http://127.0.0.1:13500',
    // Preserve the browser-facing host for the backend WebSocket Origin check.
    changeOrigin: false,
    ws: true,
  },
  '/uploads': {
    target: 'http://127.0.0.1:13500',
    changeOrigin: true,
  },
}

export default defineConfig({
  plugins: [vue()],
  server: {
    host: '0.0.0.0',
    proxy
  },
  preview: { proxy },
})
