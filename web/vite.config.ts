import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// The axum server serves ../static; in dev the Vite server proxies the API to it.
const API_TARGET = process.env.FRESHRSS_API ?? 'http://127.0.0.1:8080';

export default defineConfig({
  plugins: [svelte()],
  optimizeDeps: {
    // svelte-widgets publishes raw `.svelte` sources rather than compiled output, and resolves
    // `./theme.svelte` to `theme.svelte.js`. esbuild's pre-bundler can do neither, so the package
    // has to reach vite-plugin-svelte untouched and be compiled by our own build.
    exclude: ['svelte-widgets'],
  },
  build: {
    outDir: '../static',
    emptyOutDir: true,
    // Feeddi's reader ships a single CSS bundle; keep the same shape so cache headers and
    // the rust-embed manifest stay predictable.
    assetsDir: 'assets',
    sourcemap: false,
  },
  server: {
    port: 5173,
    strictPort: false,
    proxy: {
      '/api': { target: API_TARGET, changeOrigin: true },
      '/healthz': { target: API_TARGET, changeOrigin: true },
      '/version': { target: API_TARGET, changeOrigin: true },
    },
  },
});
