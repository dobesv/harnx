import { defineConfig, devices } from '@playwright/test';

export default defineConfig({
  testDir: './tests/e2e',
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: 'html',
  
  expect: {
    toHaveScreenshot: {
      maxDiffPixelRatio: 0.02,
    },
  },

  use: {
    baseURL: 'http://localhost:5180',
    trace: 'on-first-retry',
    animations: 'disabled',
    timezoneId: 'UTC',
  },

  projects: [
    {
      name: 'chromium-light',
      use: { ...devices['Desktop Chrome'], colorScheme: 'light' },
    },
    {
      name: 'chromium-dark',
      use: { ...devices['Desktop Chrome'], colorScheme: 'dark' },
    },
  ],

  webServer: {
    command: 'VITE_ENABLE_MSW=true pnpm dev --port 5180',
    url: 'http://localhost:5180',
    reuseExistingServer: !process.env.CI,
  },
});
