import { test, expect } from '@playwright/test';

test.beforeEach(async ({ page }) => {
  // NOTE: do NOT freeze Date.now here. The assistant-ui / @ag-ui/client runtime
  // derives message ids/ordering from the clock; a frozen clock collides the
  // optimistic user message with the streamed assistant reply and drops it.
  // Determinism for screenshots comes from the mock returning a fixed
  // session `updated_at` instead.
  await page.setViewportSize({ width: 1280, height: 720 });
});

test('initial load: shows agent picker, URL stays /', async ({ page }) => {
  await page.goto('/?scenario=happy');

  await expect(page.locator('h2')).toHaveText('Select an Agent');
  await expect(page.locator('.grid-item')).toContainText('coding/coder');
  expect(new URL(page.url()).pathname).toBe('/');
});

test('happy path: picker flow to chat with slash-named agent', async ({ page }) => {
  await page.goto('/?scenario=happy');

  await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
  await expect(page).toHaveURL(/\/agents\/coding%2Fcoder/);
  await expect(page.locator('h2')).toContainText('Sessions for coding/coder');
  await expect(page.locator('.sessions-grid')).toContainText('session-1');

  await page.locator('.new-chat-button').click();

  await expect(page.locator('.aui-status-bar')).not.toBeVisible();
  await expect(page.locator('.aui-composer-send')).toHaveText('Send');
  await expect(page).toHaveURL(/\/agents\/coding%2Fcoder\/sessions\//);

  await page.locator('.aui-composer-input').fill('Hello agent');
  await page.locator('.aui-composer-send').click();

  // The user message and the streamed assistant reply must both render in the
  // transcript (proves the stream is consumed by the runtime, not just received).
  const transcript = page.locator('.aui-thread');
  await expect(transcript).toContainText('Hello agent', { timeout: 10000 });
  await expect(transcript).toContainText('Mock streamed reply to: Hello agent', { timeout: 10000 });
  // The run finished, so the composer returns to its idle "Send" state.
  await expect(page.locator('.aui-composer-send')).toHaveText('Send');
  await expect(page).toHaveScreenshot('happy-path.png');
});

test('agents-fetch error', async ({ page }) => {
  await page.goto('/?scenario=agentsFail');

  const errorEl = page.getByTestId('agents-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/Internal Server Error/i);

  await expect(page).toHaveScreenshot('agents-fetch-error.png');
});

test('sessions-fetch error', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder?scenario=sessionsFail');

  const errorEl = page.getByTestId('sessions-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/Not Found/i);

  await expect(page).toHaveScreenshot('sessions-fetch-error.png');
});

test('send-failure error (initial send)', async ({ page }) => {
  await page.goto('/?scenario=sendFail');
  await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
  await page.locator('.new-chat-button').click();

  await page.locator('.aui-composer-input').fill('Break it!');
  await page.locator('.aui-composer-send').click();

  const errorEl = page.getByTestId('send-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/HTTP 500/i);

  await expect(page).toHaveScreenshot('send-failure-error.png');
});

test('send-failure error (queued send)', async ({ page }) => {
  await page.goto('/?scenario=happy');
  await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
  await page.locator('.new-chat-button').click();

  await page.locator('.aui-composer-input').fill('Start running');
  await page.locator('.aui-composer-send').click();
  await expect(page.locator('.aui-composer-send')).toHaveText(/Queue/i, { timeout: 1000 });

  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.sendFail);
  });

  await page.locator('.aui-composer-input').fill('Queue this failure');
  await page.locator('.aui-composer-send').click();

  const errorEl = page.getByTestId('send-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/HTTP 500/i);
});