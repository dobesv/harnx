import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  // Pre-bundle the heavier AG-UI / assistant-ui deps at server start so a cold
  // first page load doesn't trigger a mid-load dependency re-optimization (which
  // returns a 504 "Outdated Optimize Dep" and forces a reload). This keeps
  // first-load rendering — and the e2e suite — stable without any warm-up dance.
  optimizeDeps: {
    include: [
      '@assistant-ui/react',
      '@assistant-ui/react-ag-ui',
      '@ag-ui/client',
    ],
  },
  server: {
    proxy: {
      '/v1': {
        target: 'http://127.0.0.1:8000',
        changeOrigin: true,
        // Ensure SSE (Server-Sent Events) streaming works without buffering
        configure: (proxy, _options) => {
          proxy.on('proxyReq', (proxyReq, req, _res) => {
            if (req.headers.accept === 'text/event-stream') {
              proxyReq.setHeader('Cache-Control', 'no-cache')
            }
          })
          proxy.on('proxyRes', (proxyRes, req, _res) => {
            if (req.headers.accept === 'text/event-stream') {
              proxyRes.headers['cache-control'] = 'no-cache'
              proxyRes.headers['x-accel-buffering'] = 'no' // Prevent NGINX/proxy buffering
            }
          })
        }
      }
    }
  }
})
