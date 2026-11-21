import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig, type ProxyOptions } from 'vite';

// `npm run dev` proxies the API and the sockets to Kestrel's API server, so the dev server behaves
// like the embedded build. Set KESTREL_URL when the server is not on 127.0.0.1:7171.
const target = process.env.KESTREL_URL ?? 'http://127.0.0.1:7171';
// Scheme, host and port only: `http://127.0.0.1:8080/` with a slash is not a valid Origin.
const origin = new URL(target).origin;

// The API server refuses a `Host` or `Origin` that is not its own address (DNS-rebinding and
// cross-site protection), so the proxy presents the server's own origin for both.
const proxy: ProxyOptions = {
  target,
  changeOrigin: true,
  configure: (server) => {
    server.on('proxyReq', (req) => req.setHeader('origin', origin));
    server.on('proxyReqWs', (req) => req.setHeader('origin', origin));
  },
};

export default defineConfig({
  plugins: [svelte()],
  // The server mounts the UI at `/` and answers unknown paths with `index.html`, so asset URLs
  // must be absolute: a relative `./assets/...` would resolve under whatever path was requested.
  base: '/',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': proxy,
      '/ws': { ...proxy, ws: true },
    },
  },
});
