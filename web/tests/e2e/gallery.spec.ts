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
    // Verify our new mock sessions are there to ensure handlers.ts is working
    await expect(page.locator('text=session-gallery')).toBeVisible();
    await expect(page).toHaveScreenshot('2-session-picker.png');
  });

  test('3. Chat transcript', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    await page.click('text=session-gallery');
    
    // wait for messages to load (system)
    await expect(page.locator('.aui-message')).toHaveCount(1);
    
    // send a message to trigger the varied response
    await page.locator('.aui-composer-input').fill('Can you show me a tool call?');
    await page.locator('.aui-composer-send').click();
    
    // wait for the assistant response with the tool call
    await expect(page.locator('.aui-message')).toHaveCount(3); // system, user, assistant
    await expect(page.locator('.aui-tool-call')).toBeVisible();

    // expand the system message
    const details = page.locator('details.aui-system-message-details');
    await expect(details).toBeVisible();
    const isOpen = await details.evaluate((node: HTMLDetailsElement) => node.open);
    if (!isOpen) {
      await page.click('.aui-system-message-summary');
    }
    
    // verify everything is visible
    await expect(page.locator('text=You are mock system prompt')).toBeVisible();
    await expect(page.locator('text=Can you show me a tool call?')).toBeVisible();
    // Small delay to ensure any CSS transitions finish
    await page.waitForTimeout(200);

    await expect(page).toHaveScreenshot('3-chat-transcript.png');
  });

  test('4. Chat with PENDING/running state', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    await page.click('text=session-pending');
    
    // wait for messages to load (system)
    await expect(page.locator('.aui-message')).toHaveCount(1);
    
    // send a message to trigger the varied response
    await page.locator('.aui-composer-input').fill('Start a slow task');
    await page.locator('.aui-composer-send').click();
    
    // wait for status indicator to appear
    await expect(page.locator('.aui-status-indicator')).toBeVisible();
    await expect(page.locator('text=Running task...')).toBeVisible();
    
    // check composer send button is in Queue state
    await expect(page.locator('.aui-composer-send')).toHaveText('Queue');

    // Wait for the streamed partial assistant text to settle so the screenshot
    // is deterministic (the run stays live but the delta has fully arrived).
    await expect(page.locator('text=Working on it')).toBeVisible();

    // Mask the animated pulse indicator: its opacity keyframes are not frozen
    // by animations:'disabled' consistently across timing, causing flake.
    await expect(page).toHaveScreenshot('4-chat-pending.png', {
      mask: [page.locator('.aui-spinner')],
    });
  });

  test('5. Chat with NO pending message (idle, composer ready)', async ({ page }) => {
    await page.goto('/');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    
    // click new chat
    await page.click('text=New Chat');
    
    // wait for composer
    await expect(page.locator('.aui-composer')).toBeVisible();
    
    // send button should say Send
    await expect(page.locator('.aui-composer-send')).toHaveText('Send');
    
    // composer should be ready
    await expect(page.locator('.aui-composer-input')).toHaveAttribute('placeholder', 'Type a message...');
    
    await expect(page).toHaveScreenshot('5-chat-idle.png');
  });
});
