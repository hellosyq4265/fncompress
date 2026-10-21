import { defineConfig } from 'vitest/config'
import vue from '@vitejs/plugin-vue'

// Dev server on 5173 proxies /api to fncompress on 5180. The build is
// embedded in the binary with rust-embed and served from 5180 directly.
// Relative base and API URLs keep the app working behind a path prefix.
export default defineConfig({
  base: './',
  plugins: [vue()],
  server: {
    port: 5173,
    proxy: {
      '/api': 'http://127.0.0.1:5180',
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    chunkSizeWarningLimit: 1500,
  },
  test: {
    environment: 'happy-dom',
    include: ['src/**/*.test.ts'],
    // naive-ui's single-file ESM bundle loads in about a second; its
    // thousands of per-component modules took ~40 s to transform.
    alias: { 'naive-ui': 'naive-ui/dist/index.mjs' },
  },
})
