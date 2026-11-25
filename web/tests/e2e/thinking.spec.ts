import { test, expect } from '@playwright/test';

test.describe('Thinking rendering', () => {
  test('hydrated session renders thinking in collapsible block and hides literal tags', async ({ page }) => {
    await page.goto('/agents/coding%2Fcoder/sessions/session-thinking-hydrated?scenario=happy');

    // Wait for the message to hydrate
    await expect(page.locator('.aui-user-message')).toContainText('Solve this problem step by step');
    await expect(page.locator('.aui-assistant-message')).toBeVisible();

    // One collapsed thinking block is present
    const thinkingBlock = page.locator('.aui-thinking-block');
    await expect(thinkingBlock).toHaveCount(1);
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

    // Reload preserves one collapsed thinking block, clean answer, and full reasoning text
    await page.reload();
    await expect(page.locator('.aui-assistant-message')).toBeVisible();
    await expect(thinkingBlock).toHaveCount(1);
    await expect(thinkingBlock).not.toHaveAttribute('open', '');
    const reloadedText = await assistantMessage.innerText();
    expect(reloadedText).not.toContain('<think>');
    expect(reloadedText).not.toContain('</think>');
    expect(reloadedText).toContain('Here is the complete solution to your problem.');
    await summary.click();
    await expect(thinkingContent).toContainText('First analyze the constraints.');
    await expect(thinkingContent).toContainText('Then construct the proof.');
  });

  test('legacy inline tagged session renders collapsed thinking block as fallback', async ({ page }) => {
    await page.goto('/agents/coding%2Fcoder/sessions/session-thinking-legacy?scenario=happy');

    await expect(page.locator('.aui-user-message')).toContainText('Solve this problem step by step');
    await expect(page.locator('.aui-assistant-message')).toBeVisible();

    const thinkingBlock = page.locator('.aui-thinking-block');
    await expect(thinkingBlock).toHaveCount(1);
    await expect(thinkingBlock).not.toHaveAttribute('open', '');

    const assistantText = await page.locator('.aui-assistant-message').innerText();
    expect(assistantText).not.toContain('<think>');
    expect(assistantText).not.toContain('</think>');
    expect(assistantText).toContain('Here is the complete solution to your problem.');

    await page.locator('.aui-thinking-summary').click();
    await expect(thinkingBlock).toHaveAttribute('open', '');
    await expect(page.locator('.aui-thinking-content')).toContainText('First analyze the constraints.');
    await expect(page.locator('.aui-thinking-content')).toContainText('Then construct the proof.');
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

    // Wait for stream to complete, assistant message to arrive, and background snapshot sync to settle
    await expect(page.locator('.aui-assistant-message')).toContainText('Here is the final answer after live reasoning.');
    await expect(page.locator('.aui-composer-send')).toHaveText('Send');
    await expect(page.locator('.aui-assistant-message').first()).toHaveAttribute('data-message-id', /^mock-assistant-/);

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

    // Wait for follow-up turn to finish cleanly before navigating away
    await expect(page.locator('.aui-composer-input')).toHaveValue('');
    await expect(page.locator('.aui-error')).not.toBeVisible();
    await expect(page.locator('.aui-user-message').last()).toContainText('follow up question');
    await expect(page.locator('.aui-thread')).toContainText('Mock streamed reply to: follow up question', { timeout: 10000 });
    await expect(page.locator('.aui-composer-send')).toHaveText('Send');

    // Re-hydrating the session via SPA navigation renders one collapsed thinking block from history_reasoning
    await page.goBack();
    await expect(page.locator('.session-item', { hasText: 'Fix login redirect issue' })).toBeVisible();
    await page.goForward();
    await expect(page.locator('.aui-assistant-message').first()).toContainText('Here is the final answer after live reasoning.');
    await expect(thinkingBlock).toHaveCount(1);
    await expect(thinkingBlock).not.toHaveAttribute('open', '');
    await page.locator('.aui-thinking-summary').click();
    await expect(content).toContainText('Thinking through the approach in real time...');
  });
});
