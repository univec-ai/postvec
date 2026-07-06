import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { resolve } from 'path'
import { fileURLToPath } from 'url'

const __dirname = fileURLToPath(new URL('.', import.meta.url))
const target = process.env.VITE_API_ENDPOINT || 'http://127.0.0.1:22222'
const proxy = Object.fromEntries(
  ['/api', '/config', '/health', '/ready'].map((path) => [path, { target, changeOrigin: true }]),
)

export default defineConfig({
  plugins: [react()],
  resolve: { alias: { '@': resolve(__dirname, 'src') } },
  server: { port: 3000, host: true, proxy },
  build: {
    outDir: 'dist',
    sourcemap: true,
    rollupOptions: { output: { manualChunks: { 'react-vendor': ['react', 'react-dom'] } } },
  },
})
