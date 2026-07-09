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
          // Charts libs (@mantine/charts + its recharts subtree) are only
          // reached through the lazy Ops route. Return undefined so rolldown
          // places them in the lazy Ops chunk itself. Forcing them into ANY
          // named chunk — even a dedicated 'charts' — makes an eager chunk
          // (framework/mantine) import a shared submodule back out of it,
          // which drags the whole ~400 kB charts graph into the initial load
          // and defeats the route split. Verified: with undefined here,
          // index.html carries no charts modulepreload and the entry has no
          // static charts import.
          if (
            id.includes('/node_modules/@mantine/charts/') ||
            id.includes('/node_modules/recharts/')
          ) {
            return undefined
          }
          if (/\/node_modules\/react(?:-dom)?\//.test(id) || /\/node_modules\/react-router\//.test(id)) {
            return 'framework'
          }
          if (id.includes('/node_modules/@mantine/')) {
            return 'mantine'
          }
          if (id.includes('/node_modules/@tanstack/react-query/')) {
            return 'query'
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
