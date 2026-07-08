import { test, expect } from '@playwright/test';

test.beforeEach(async ({ page }) => {
  // Make the UI deterministic
  await page.addInitScript(() => {
    // Freeze Date to make the relative times stable (if any are used in Assistant-UI)
    const fixedTime = new Date('2024-01-01T12:00:00Z').getTime();
    Date.now = () => fixedTime;
  });
  await page.setViewportSize({ width: 1280, height: 720 });
});

test('happy path: chat with slash-named agent', async ({ page }) => {
  await page.goto('/?scenario=happy');

  // Should have mock assistant agent available
  await expect(page.locator('select')).toHaveValue('coding/coder');
  
  // Sessions list should contain 'session-1'
  await expect(page.locator('.sessions-list')).toContainText('session-1');

  // Send a message
  await page.locator('.aui-composer').locator('textarea').fill('Hello agent');
  await page.locator('.aui-composer-send').click();

  // Wait for the response status text to appear
  await expect(page.locator('.aui-status-indicator')).toContainText('Mock stream finished');

  // Take screenshot
  await expect(page).toHaveScreenshot('happy-path.png');
});

test('agents-fetch error', async ({ page }) => {
  await page.goto('/?scenario=agentsFail');
  
  // The error element should be visible
  const errorEl = page.getByTestId('agents-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/Internal Server Error/i);

  await expect(page).toHaveScreenshot('agents-fetch-error.png');
});

test('sessions-fetch error', async ({ page }) => {
  await page.goto('/?scenario=sessionsFail');
  
  // The error element should be visible
  const errorEl = page.getByTestId('sessions-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/Not Found/i);

  await expect(page).toHaveScreenshot('sessions-fetch-error.png');
});

test('send-failure error (initial send)', async ({ page }) => {
  await page.goto('/?scenario=sendFail');

  // Make sure it loaded the initial list
  await expect(page.locator('select')).toHaveValue('coding/coder');

  // Type and send a message
  await page.locator('.aui-composer').locator('textarea').fill('Break it!');
  await page.locator('.aui-composer-send').click();

  // The send-error element should appear
  const errorEl = page.getByTestId('send-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/RPC call failed/i);

  await expect(page).toHaveScreenshot('send-failure-error.png');
});

test('send-failure error (queued send)', async ({ page }) => {
  await page.goto('/?scenario=happy');

  // Load app, start a happy-path stream which is slow (50ms per chunk).
  await expect(page.locator('select')).toHaveValue('coding/coder');
  
  // Type and send first message to get into isRunning state
  await page.locator('.aui-composer').locator('textarea').fill('Start running');
  await page.locator('.aui-composer-send').click();

  // Make sure we are in isRunning state (the composer should show 'Queue' or similar)
  // Our CSS disables the input when running except for queuing
  await expect(page.locator('.aui-composer-send')).toHaveText(/Queue/i, { timeout: 1000 });

  // Now, inject the sendFail scenario dynamically
  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.sendFail);
  });

  // Type a second message while it's still running, to hit MyComposer.handleSubmit
  await page.locator('.aui-composer').locator('textarea').fill('Queue this failure');
  await page.locator('.aui-composer-send').click();

  // The send-error element should appear inline
  const errorEl = page.getByTestId('send-error');
  await expect(errorEl).toBeVisible();
  await expect(errorEl).toHaveText(/RPC call failed/i);
});
