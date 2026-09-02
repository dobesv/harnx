import { test, expect } from '@playwright/test';

test.beforeEach(async ({ page }) => {
  await page.setViewportSize({ width: 1280, height: 720 });
});

test('composer active-session view', async ({ page }) => {
  // 1. No header: on the active-session view
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');

  await expect(page.locator('.top-nav')).toHaveCount(0);
  await expect(page.locator('.aui-composer')).toBeVisible();

  // 2. Agent switch link is a real <a href="/"> doing IN-PAGE SPA nav
  await page.evaluate(() => ((window as any).__noReload = true));
  
  const agentTrigger = page.locator('button[aria-label^="Agent:"]');
  await agentTrigger.click();

  const switchAgentItem = page.getByRole('menuitem', { name: /switch agent/i });
  await expect(switchAgentItem).toHaveAttribute('href', '/');
  await switchAgentItem.click();

  await expect(page.locator('h2', { hasText: 'Select an Agent' })).toBeVisible();
  expect(await page.evaluate(() => (window as any).__noReload)).toBe(true);
  expect(new URL(page.url()).pathname).toBe('/');

  // 3. Session switch link is <a href="/agents/coding%2Fcoder"> doing in-page SPA nav
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');
  await page.evaluate(() => ((window as any).__noReload = true));
  
  const sessionTrigger = page.locator('button[aria-label^="Session:"]');
  await sessionTrigger.click();

  const switchSessionItem = page.getByRole('menuitem', { name: /switch session/i });
  await expect(switchSessionItem).toHaveAttribute('href', '/agents/coding%2Fcoder');
  await switchSessionItem.click();

  await expect(page.locator('h2', { hasText: 'Sessions for coding/coder' })).toBeVisible();
  expect(await page.evaluate(() => (window as any).__noReload)).toBe(true);
  expect(new URL(page.url()).pathname).toBe('/agents/coding%2Fcoder');
});

test('composer keyboard navigation', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');
  
  const agentTrigger = page.locator('button[aria-label^="Agent:"]');
  await agentTrigger.focus();
  await page.keyboard.press('ArrowDown');
  
  const menu = page.getByRole('menu');
  await expect(menu).toBeVisible();

  await page.keyboard.press('Escape');
  await expect(menu).not.toBeVisible();
  await expect(agentTrigger).toBeFocused();
});

test('composer responsive boundary', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');

  const desktopControls = page.locator('.aui-composer-controls-desktop');
  const mobileControls = page.locator('.aui-composer-controls-mobile');

  // At 769px
  await page.setViewportSize({ width: 769, height: 720 });
  await expect(desktopControls).toBeVisible();
  await expect(mobileControls).toBeHidden();

  // At 768px
  await page.setViewportSize({ width: 768, height: 720 });
  await expect(desktopControls).toBeHidden();
  await expect(mobileControls).toBeVisible();
});

test('composer screenshots', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');
  
  // Desktop
  await page.setViewportSize({ width: 1280, height: 720 });
  // Explicitly wait for composer controls and UI to settle
  await expect(page.locator('.aui-composer')).toBeVisible();
  await expect(page.locator('.aui-composer-controls-desktop')).toBeVisible();
  // Wait for session data to render properly (e.g. messages visible)
  await expect(page.locator('.aui-message').first()).toBeVisible();
  await expect(page).toHaveScreenshot('composer-desktop.png');

  // Mobile
  await page.setViewportSize({ width: 375, height: 667 });
  await expect(page.locator('.aui-composer-controls-mobile')).toBeVisible();
  await expect(page).toHaveScreenshot('composer-mobile.png');
});
