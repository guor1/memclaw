import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

export default defineConfig({
  plugins: [vue()],
  build: {
    // rust-embed reads from this folder; keep it stable.
    outDir: 'dist',
    emptyOutDir: true,
    // Inline everything small enough to avoid a wall of tiny asset requests
    // over what may be a phone-to-laptop LAN connection.
    assetsInlineLimit: 8192,
  },
  server: {
    // `npm run dev` proxies API calls to a locally running `oc http`, so the
    // UI can hot-reload without rebuilding the Rust binary.
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/health': 'http://127.0.0.1:8080',
    },
  },
})
