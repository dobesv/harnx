import { test, expect } from '@playwright/test';

test('loads existing session from snapshot stream without history GET', async ({ page }) => {
  const historyRequests: string[] = [];
  await page.route('**/v1/agents/**/sessions/session-1', async (route) => {
    if (route.request().method() === 'GET') {
      historyRequests.push(route.request().url());
    }
    await route.continue();
  });

  await page.goto('/?scenario=happy');
  await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
  await page.locator('.session-item').filter({ hasText: 'session-1' }).click();

  // Transcript hydrates from the stream's MESSAGES_SNAPSHOT (wait for it first —
  // it is the durable outcome of the mount subscribe run).
  await expect(page.locator('.aui-message').first()).toBeVisible({ timeout: 10000 });
  await expect(page.locator('.aui-message-content')).toContainText(['Hello from mock session']);
  await expect(page.locator('.aui-system-message-summary')).toHaveText('System prompt ▸');
  // Idle after hydration (the subscribe run finished; nothing generating).
  await expect(page.locator('.aui-status-bar')).not.toBeVisible();
  await expect(page.locator('.aui-composer-send')).toHaveText('Send');
  // Hydration came from the stream, NOT a side GET history request.
  expect(historyRequests).toEqual([]);
});