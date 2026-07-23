import { test, expect } from '@playwright/test';

test.describe('Gallery', () => {
  test('1. Agent picker', async ({ page }) => {
    await page.goto('/');
    await expect(page.locator('.picker-container h2').first()).toHaveText('Select an Agent');
    await expect(page.locator('.grid-list').first()).toBeVisible();
    await expect(page).toHaveScreenshot('1-agent-picker.png');
  });

  test('2. Session picker', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    await expect(page.locator('.picker-container h2').first()).toHaveText('Sessions for coding/coder');
    await expect(page.locator('.grid-list').first()).toBeVisible();
    await expect(page.locator('text=session-gallery')).toBeVisible();
    await expect(page).toHaveScreenshot('2-session-picker.png');
  });

  test('3. Chat transcript (GFM table, collapsed tool card, status bar usage, active session density)', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    await page.click('text=session-gallery');

    // Wait for a snapshot-specific signal, not a generic message count: the
    // system message from the promptless-subscribe MESSAGES_SNAPSHOT renders the
    // collapsed "System prompt ▸" summary. A bare toHaveCount(1) can transiently
    // match assistant-ui's empty placeholder before hydration and flake. Then
    // wait for the composer to reach idle ("Send") so the subscribe run settled.
    await expect(page.locator('.aui-system-message-summary')).toHaveText('System prompt ▸', { timeout: 30000 });
    await expect(page.locator('.aui-composer-send')).toHaveText('Send', { timeout: 30000 });
    await expect(page.locator('.aui-message')).toHaveCount(1);

    await page.locator('.aui-composer-input').fill('Can you show me a tool call and a table?');
    await page.locator('.aui-composer-send').click();

    await expect(page.locator('.aui-message')).toHaveCount(3); // system, user, assistant

    // (a) Markdown message with GFM table (should be visible)
    await expect(page.locator('.aui-message table')).toBeVisible();

    // (b) Tool card COLLAPSED + summary
    await expect(page.locator('.aui-tool-call')).toBeVisible();
    await expect(page.locator('.aui-tool-summary >> text=Fetched data from API.')).toBeVisible();

    // (d) Status bar showing completion usage
    await expect(page.locator('.aui-status-bar')).toBeVisible();
    await expect(page.locator('.aui-status-usage-item[aria-label="Input tokens: 100"]')).toBeVisible();
    await expect(page.locator('.aui-status-usage-item[aria-label="Context usage: 300 (30%)"]')).toBeVisible();

    // Small delay to ensure any CSS transitions finish
    await page.waitForTimeout(200);

    // This captures active session density, collapsed tool card, usage status bar, and markdown table
    await expect(page).toHaveScreenshot('3-chat-transcript.png');
  });

  test('3b. Expanded Tool Card (JSON tree + view-source)', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    await page.click('text=session-gallery');

    // Wait for the initial promptless subscribe to hydrate before interacting.
    // Gate on the snapshot-specific "System prompt ▸" summary (only rendered once
    // the system message from MESSAGES_SNAPSHOT is applied) rather than a generic
    // .aui-message visibility, which can transiently match an empty placeholder.
    // Then wait for the composer to reach idle ("Send") so the run has settled.
    await expect(page.locator('.aui-system-message-summary')).toHaveText('System prompt ▸', { timeout: 30000 });
    await expect(page.locator('.aui-composer-send')).toHaveText('Send', { timeout: 30000 });

    await page.locator('.aui-composer-input').fill('Trigger tool');
    await page.locator('.aui-composer-send').click();

    await expect(page.locator('.aui-message')).toHaveCount(3);
    await expect(page.locator('.aui-tool-call')).toBeVisible();

    // Click to expand tool card
    await page.click('.aui-tool-call-header');

    // Verify tree view defaults (button says View Source when tree is visible)
    await expect(page.locator('button:has-text("View Source")')).toBeVisible();

    // Switch to View Source
    await page.click('button:has-text("View Source")');
    await expect(page.locator('text=Raw Args:')).toBeVisible();
    await expect(page.locator('.aui-tool-call-body')).toContainText('"query": "example"');

    await page.waitForTimeout(200);
    // (c) tool card EXPANDED showing view-source
    await expect(page).toHaveScreenshot('3b-tool-expanded.png');
  });

  test('4. Chat with PENDING/running state', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    await page.click('text=session-pending');

    await expect(page.locator('.aui-message')).toHaveCount(1);

    // The session-pending mock starts a run (RUN_STARTED) that never finishes,
    // so the composer enters its "running" state. We MUST wait for that state to
    // be established before typing/clicking; otherwise the click can land while
    // isRunning is still false and submit a new run instead of queuing, leaving
    // the button on "Queue" instead of "Queued". Gate on the running-status text
    // (emitted right after RUN_STARTED) AND the button flipping to "Queue".
    await expect(page.locator('.aui-status-bar')).toBeVisible({ timeout: 30000 });
    await expect(page.locator('text=Running task...')).toBeVisible({ timeout: 30000 });
    await expect(page.locator('.aui-composer-send')).toHaveText('Queue', { timeout: 30000 });

    await page.locator('.aui-composer-input').fill('Start a slow task');
    await page.locator('.aui-composer-send').click();

    // Clicking Send while a run is active queues the message → button "Queued".
    await expect(page.locator('.aui-composer-send')).toHaveText('Queued', { timeout: 30000 });

    await expect(page.locator('text=Working on it')).toBeVisible();

    await expect(page).toHaveScreenshot('4-chat-pending.png', {
      mask: [page.locator('.aui-spinner')],
    });
  });

  test('5. Empty session composer', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();

    await page.click('text=New Chat');

    await expect(page.locator('.aui-composer-container')).toBeVisible();

    await expect(page.locator('.aui-composer-send')).toHaveText('Send', { timeout: 30000 });

    await expect(page.locator('.aui-composer-input')).toHaveAttribute('placeholder', 'Type a message...');

    page.on('console', msg => console.log('BROWSER CONSOLE:', msg.text()));
    await expect(page).toHaveScreenshot('5-chat-idle.png');
  });

  test('6. Restored session tool cards', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();

    // Open the restored session
    await page.click('text=session-restored');

    // Wait for the tool summary to show up from snapshot markdown (fallback to command args)
    await expect(page.locator('.aui-tool-call')).toBeVisible();
    await expect(page.locator('text=$ ls -la')).toBeVisible();

    // Ensure promptless subscribe has fully settled so composer says 'Send' in the screenshot
    await expect(page.locator('.aui-composer-send')).toHaveText('Send', { timeout: 30000 });

    await page.waitForTimeout(200);

    // (g) restored-session showing tool summary cards after reload
    await expect(page).toHaveScreenshot('6-restored-session.png');
  });
});
