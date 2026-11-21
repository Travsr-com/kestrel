// The M2 browser smoke suite. `tests/ui.rs` starts a real `kestrel up` and runs this with the
// server URL, the API token and the run ids in the environment (see `tests/kestrel.ts`).
import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests',
  // One browser, one test at a time: both tests use the same live server.
  fullyParallel: false,
  workers: 1,
  retries: 0,
  forbidOnly: !!process.env.CI,
  // A tree of gVisor sandboxes takes a while to grow and finish.
  timeout: 300_000,
  expect: { timeout: 30_000 },
  reporter: [['list']],
  outputDir: 'test-results',
  use: {
    baseURL: process.env.KESTREL_URL,
    headless: true,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
