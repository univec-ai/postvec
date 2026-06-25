import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import { transform } from 'esbuild'
import { resolve } from 'path'
import { fileURLToPath } from 'url'

const __dirname = fileURLToPath(new URL('.', import.meta.url))

function jsxInJs() {
  return {
    name: 'jsx-in-js',
    enforce: 'pre',
    async transform(code, id) {
      if (!id.includes('/src/shared/') || !id.endsWith('.js')) {
        return null
      }
      const result = await transform(code, {
        loader: 'jsx',
        jsx: 'automatic',
        sourcefile: id,
      })
      return { code: result.code, map: null }
    },
  }
}

export default defineConfig({
  plugins: [jsxInJs(), react()],
  optimizeDeps: {
    esbuildOptions: {
      loader: { '.js': 'jsx' },
    },
    include: ['react', 'react-dom', 'zustand', 'immer'],
  },
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
      '@components': resolve(__dirname, 'src/components'),
      '@shared': resolve(__dirname, 'src/shared'),
      '@store': resolve(__dirname, 'src/store'),
    },
  },
  server: {
    port: 3000,
    host: true,
    proxy: {
      '/api': {
        target: process.env.VITE_API_ENDPOINT || 'http://127.0.0.1:22222',
        changeOrigin: true,
      },
      '/config': {
        target: process.env.VITE_API_ENDPOINT || 'http://127.0.0.1:22222',
        changeOrigin: true,
      },
      '/health': {
        target: process.env.VITE_API_ENDPOINT || 'http://127.0.0.1:22222',
        changeOrigin: true,
      },
      '/ready': {
        target: process.env.VITE_API_ENDPOINT || 'http://127.0.0.1:22222',
        changeOrigin: true,
      },
    },
  },
  define: {
    global: 'globalThis',
  },
  build: {
    outDir: 'dist',
    sourcemap: true,
    rollupOptions: {
      output: {
        manualChunks: {
          'react-vendor': ['react', 'react-dom'],
        },
      },
    },
  },
})
