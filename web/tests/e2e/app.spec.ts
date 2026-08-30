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
  await expect(page).toHaveURL(/\/agents\/coding%2Fcoder\/sessions\/aMock1/);

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

test('committed handoff navigates and hydrates the durable target session', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');
  await expect(page.locator('.aui-assistant-message')).toContainText('Hello from mock session');

  await page.locator('.aui-composer-input').fill('handoff now');
  await page.locator('.aui-composer-send').click();

  await expect(page).toHaveURL(/\/agents\/assistant\/sessions\/handoff-target/);
  await expect(page.locator('.aui-user-message')).toContainText(
    'Delegated work from coding/coder',
  );
  await expect(page.locator('.aui-assistant-message')).toContainText(
    'Durable handoff target history',
  );
  await expect(page.locator('.aui-composer-send')).toHaveText('Send');
});

test('sub-agent row transitions, opens the child, and browser Back returns to the parent', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');
  await expect(page.locator('.aui-assistant-message')).toContainText('Hello from mock session');
  await expect(page.locator('.aui-composer-send')).toHaveText('Send');

  await page.locator('.aui-composer-input').fill('delegate to researcher');
  await page.locator('.aui-composer-send').click();

  const childRows = page.getByRole('button', {
    name: /Open researcher sub-agent session child-session-0001/,
  });
  const childRow = childRows.first();
  await expect(childRow).toHaveAttribute('data-status', 'running');
  await expect(childRow).toContainText('child-session-0001');
  await expect(childRow).toHaveAttribute('data-status', 'done', { timeout: 10000 });

  await page.locator('.aui-composer-input').fill('delegate to researcher');
  await page.locator('.aui-composer-send').click();
  await expect(childRows).toHaveCount(2);
  await expect(childRows.last()).toHaveAttribute('data-status', 'running');
  await expect(childRows.last()).toHaveAttribute('data-status', 'done', { timeout: 10000 });

  await childRows.last().click();
  await expect(page).toHaveURL(/\/agents\/researcher\/sessions\/child-session-0001/);
  await expect(page.locator('.aui-user-message')).toContainText('Research this task');
  await expect(page.locator('.aui-assistant-message')).toContainText('Child task complete.');

  await page.goBack();
  await expect(page).toHaveURL(/\/agents\/coding%2Fcoder\/sessions\/session-1/);
  await expect(page.locator('.aui-user-message')).toHaveCount(2);
  await expect(page.getByRole('button', {
    name: /Open researcher sub-agent session child-session-0001 \(done\)/,
  })).toHaveCount(2);
});

test('composer: no scrollbar until max-height, resets after send', async ({ page }) => {
  await page.goto('/?scenario=happy');
  await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
  await page.locator('.new-chat-button').click();

  const input = page.locator('.aui-composer-input');

  // A few lines: textarea has grown but is still under the 50vh cap, so the
  // scrollbar must stay hidden (the phantom-scrollbar bug being fixed).
  await input.fill('line 1\nline 2\nline 3');
  await expect(input).toHaveCSS('overflow-y', 'hidden');
  const grownHeight = await input.evaluate((el) => (el as HTMLTextAreaElement).clientHeight);

  // Enough lines to exceed max-height (50vh of a 720px viewport = 360px).
  // The height is capped and the scrollbar becomes functional (overflow auto).
  await input.fill(Array.from({ length: 60 }, (_, i) => `row ${i}`).join('\n'));
  await expect(input).toHaveCSS('overflow-y', 'auto');
  const cappedHeight = await input.evaluate((el) => (el as HTMLTextAreaElement).clientHeight);
  expect(cappedHeight).toBeLessThanOrEqual(360);
  expect(cappedHeight).toBeGreaterThan(grownHeight);

  // After sending, the composer collapses back to single-line, no scrollbar.
  await page.locator('.aui-composer-send').click();
  await expect(input).toHaveCSS('overflow-y', 'hidden');
  await expect
    .poll(async () => input.evaluate((el) => (el as HTMLTextAreaElement).clientHeight))
    .toBeLessThan(cappedHeight);
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
  await expect(errorEl.getByRole('button', { name: 'Retry' })).toBeVisible();

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
