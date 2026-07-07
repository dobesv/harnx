import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
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
