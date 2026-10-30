import { test, expect } from '@playwright/test';

test.describe('Thinking rendering', () => {
  test('hydrated session renders thinking in collapsible block and hides literal tags', async ({ page }) => {
    await page.goto('/agents/coding%2Fcoder/sessions/session-thinking-hydrated?scenario=happy');

    // Wait for the message to hydrate
    await expect(page.locator('.aui-user-message')).toContainText('Solve this problem step by step');
    await expect(page.locator('.aui-assistant-message')).toBeVisible();

    // The thinking block is present
    const thinkingBlock = page.locator('.aui-thinking-block');
    await expect(thinkingBlock).toBeVisible();

    // Summary should show "Thinking"
    const summary = page.locator('.aui-thinking-summary');
    await expect(summary).toContainText('Thinking');

    // Collapsed by default
    await expect(thinkingBlock).not.toHaveAttribute('open', '');

    // Neither <think> nor </think> appears in DOM text
    const assistantMessage = page.locator('.aui-assistant-message');
    const fullText = await assistantMessage.innerText();
    expect(fullText).not.toContain('<think>');
    expect(fullText).not.toContain('</think>');
    expect(fullText).toContain('Here is the complete solution to your problem.');

    // Screenshot of collapsed state
    await expect(page).toHaveScreenshot('thinking-hydrated-collapsed.png');

    // Expanding shows the content
    await summary.click();
    await expect(thinkingBlock).toHaveAttribute('open', '');
    const thinkingContent = page.locator('.aui-thinking-content');
    await expect(thinkingContent).toBeVisible();
    await expect(thinkingContent).toContainText('First analyze the constraints.');
    await expect(thinkingContent).toContainText('Then construct the proof.');

    // Screenshot of expanded state
    await expect(page).toHaveScreenshot('thinking-hydrated-expanded.png');
  });

  test('live streaming thinking renders open then collapses automatically when streaming finishes', async ({ page }) => {
    await page.goto('/?scenario=happy');
    await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
    await page.locator('.new-chat-button').click();

    // Wait for the composer to be ready
    await expect(page.locator('.aui-composer-send')).toHaveText('Send');

    // Send a prompt that triggers thinking
    await page.locator('.aui-composer-input').fill('think then answer');
    await page.locator('.aui-composer-send').click();

    // Wait for stream to complete and assistant message to arrive
    await expect(page.locator('.aui-assistant-message')).toContainText('Here is the final answer after live reasoning.');

    // Thinking block is present
    const thinkingBlock = page.locator('.aui-thinking-block');
    await expect(thinkingBlock).toBeVisible();

    // Collapses automatically when streaming finishes
    await expect(thinkingBlock).not.toHaveAttribute('open', '');

    // No raw tags
    const assistantText = await page.locator('.aui-assistant-message').innerText();
    expect(assistantText).not.toContain('<think>');
    expect(assistantText).not.toContain('</think>');

    // Opening it manually reveals content
    await page.locator('.aui-thinking-summary').click();
    await expect(thinkingBlock).toHaveAttribute('open', '');
    const content = page.locator('.aui-thinking-content');
    await expect(content).toContainText('Thinking through the approach in real time...');

    // Now test follow-up prompt: sending a second prompt after reasoning
    await page.locator('.aui-composer-input').fill('follow up question');
    await page.locator('.aui-composer-send').click();

    // Wait for composer to settle and verify no errors
    await expect(page.locator('.aui-composer-input')).toHaveValue('');
    await expect(page.locator('.aui-error')).not.toBeVisible();
    await expect(page.locator('.aui-user-message').last()).toContainText('follow up question');
  });
});
