import { readFileSync } from 'node:fs'
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

const backendUrl = process.env.VITE_DEV_BACKEND_URL ?? 'http://localhost:8484'
let cargoVersion = process.env.APP_VERSION ?? '0.0.0'

try {
  const cargoToml = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8')
  cargoVersion = cargoToml.match(/\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1] ?? cargoVersion
} catch {
  // Some build environments only mount the frontend subtree.
}

export default defineConfig({
  plugins: [react()],
  define: {
    __APP_VERSION__: JSON.stringify(`v${cargoVersion}`),
  },
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
          // @mantine/charts must not join the eager 'mantine' chunk: it is
          // only imported by the lazy Ops route, and grouping it with
          // @mantine/core would drag it (and its recharts subtree) into the
          // initial bundle, defeating the route-level code split.
          if (id.includes('/node_modules/@mantine/charts/')) {
            return 'charts'
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
