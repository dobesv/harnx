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
  await expect(page.locator('.aui-assistant-message')).toContainText('Hello from mock session');
  await expect(page.locator('.aui-system-message-summary')).toHaveText('System prompt ▸');
  // Idle after hydration (the subscribe run finished; nothing generating).
  await expect(page.locator('.aui-status-bar')).not.toBeVisible();
  await expect(page.locator('.aui-composer-send')).toHaveText('Send');
  // Hydration came from the stream, NOT a side GET history request.
  expect(historyRequests).toEqual([]);
});

test('user and assistant messages have distinct role styling hooks', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-restored?scenario=happy');

  await expect(page.locator('.aui-user-message')).toContainText('Show me restored tool call');
  await expect(page.locator('.aui-assistant-message')).toBeVisible();
});

test('another tab receives session updates without reloading', async ({ context }) => {
  const first = await context.newPage();
  const second = await context.newPage();
  const url = '/agents/coding%2Fcoder/sessions/session-1?scenario=happy';

  await Promise.all([first.goto(url), second.goto(url)]);
  await expect(first.locator('.aui-composer-send')).toHaveText('Send');
  await expect(second.locator('.aui-composer-send')).toHaveText('Send');

  await first.locator('.aui-composer-input').fill('message from first tab');
  await first.locator('.aui-composer-send').click();

  await expect(second.locator('.aui-thread')).toContainText('message from first tab', {
    timeout: 10000,
  });
  await expect(second.locator('.aui-thread')).toContainText(
    'Mock streamed reply to: message from first tab',
    { timeout: 10000 },
  );
});
