import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Development: the API and the realtime WebSocket are proxied to a running
// Farwing Server (`FARWING_SERVER`, default the local test server). In
// production the server serves the built files itself.
const server = process.env.FARWING_SERVER ?? 'https://localhost:18443'

export default defineConfig({
  plugins: [react()],
  // Relative asset URLs resolve against <base href>, which the server sets
  // to its public path (a reverse proxy may serve it under a prefix).
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
  },
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      '/api': { target: server, changeOrigin: true, secure: false, ws: true },
      '/healthz': { target: server, changeOrigin: true, secure: false },
    },
  },
})
