import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

const backendUrl = process.env.VITE_DEV_BACKEND_URL ?? 'http://localhost:8484'

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: '../static',
    emptyOutDir: true,
    rolldownOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes('/node_modules/')) return undefined
          if (/\/node_modules\/react(?:-dom)?\//.test(id) || /\/node_modules\/react-router\//.test(id)) {
            return 'framework'
          }
          if (id.includes('/node_modules/@mantine/')) {
            return 'mantine'
          }
          if (id.includes('/node_modules/@tanstack/react-query/')) {
            return 'query'
          }
          if (id.includes('/node_modules/recharts/')) {
            return 'charts'
          }
          if (id.includes('/node_modules/@tabler/icons-react/')) {
            return 'icons'
          }
          return 'vendor'
        },
      },
    },
  },
  server: {
    proxy: {
      '/api': backendUrl,
      '/auth': backendUrl,
    },
  },
})
