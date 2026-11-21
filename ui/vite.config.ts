import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vite';

// `npm run dev` proxies the API and the sockets to Kestrel's API server, so the dev server behaves
// like the embedded build. Set KESTREL_URL when the server is not on 127.0.0.1:7171.
const target = process.env.KESTREL_URL ?? 'http://127.0.0.1:7171';

export default defineConfig({
  plugins: [svelte()],
  // Relative asset paths, so the build works wherever the server mounts it.
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': { target, changeOrigin: false },
      '/ws': { target, ws: true, changeOrigin: false },
    },
  },
});
