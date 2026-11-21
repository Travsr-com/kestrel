import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [svelte(), svelteTesting()],
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
