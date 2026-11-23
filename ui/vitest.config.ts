import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  // Components inject their own `<style>` when they mount, so a test sees what a component's
  // scoped rules do (#84: the token field's own rule hid it). The app build keeps external CSS.
  plugins: [svelte({ compilerOptions: { css: 'injected' } }), svelteTesting()],
  test: {
    environment: 'jsdom',
    include: ['tests/**/*.test.ts'],
    setupFiles: ['tests/setup.ts'],
    coverage: {
      provider: 'v8',
      include: ['src/**'],
      exclude: [
        // The entry file only mounts <App> on the page; App and everything it uses is tested.
        'src/main.ts',
        // Type declarations: no code to run.
        'src/**/*.d.ts',
      ],
      reporter: ['text', 'text-summary'],
      thresholds: {
        lines: 95,
      },
    },
  },
});
