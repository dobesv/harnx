import { test, expect } from '@playwright/test';

test.beforeEach(async ({ page }) => {
  // Compress backoff delays to tens of ms for fast E2E test runs
  await page.addInitScript(() => {
    (window as any).__harnxConnection = {
      initialDelayMs: 25,
      maxDelayMs: 50,
    };
  });
  await page.setViewportSize({ width: 1280, height: 720 });
});

test('Scenario 1: Initial load with agents 502 blip shows connecting state and auto-recovers when backend returns without reload', async ({ page }) => {
  // Start with agents transient 502
  await page.goto('/?scenario=agentsTransient502');

  // Assert blocking connecting state is shown with role="status"
  const connecting = page.getByTestId('agents-connecting');
  await expect(connecting).toBeVisible();
  await expect(connecting).toHaveAttribute('role', 'status');

  // Verify non-blocking banner is suppressed on initial AgentPicker before data
  await expect(page.getByTestId('connection-banner')).not.toBeVisible();

  // Flip mock handler to happy path mid-test without page reload
  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.happy);
  });

  // Automatically recovers without page reload
  await expect(page.locator('h2')).toHaveText('Select an Agent');
  await expect(page.locator('.grid-item')).toContainText('coding/coder');
  await expect(connecting).not.toBeVisible();
});

test('Scenario 2: Background failure with data already loaded shows non-blocking banner and clears on recovery', async ({ page }) => {
  // Load initially with agents and sessions
  await page.goto('/?scenario=happy');
  await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
  await expect(page.locator('h2')).toContainText('Sessions for coding/coder');
  await expect(page.locator('.sessions-grid')).toContainText('session-1');

  // Simulate background backend outage (502 on sessions)
  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.sessionsTransient502);
  });

  // Trigger a real background read attempt for sessions
  await page.evaluate(async () => {
    const { listSessions } = await import('/src/api.ts');
    // Calling listSessions triggers the retrying loop and connection coordinator
    void listSessions('coding/coder');
  });

  // Assert non-blocking banner appears at the top
  const banner = page.getByTestId('connection-banner');
  await expect(banner).toBeVisible();
  await expect(banner).toHaveAttribute('role', 'status');
  await expect(banner).toContainText('Having trouble reaching the server');

  // Existing sessions content stays visible and interactive (not unmounted by loading state)
  await expect(page.locator('.sessions-grid')).toContainText('session-1');
  await expect(page.getByRole('button', { name: 'New Chat' })).toBeEnabled();

  // Flip mock back to happy path
  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.happy);
  });

  // Banner clears automatically on retry recovery
  await expect(banner).not.toBeVisible();
  await expect(page.locator('.sessions-grid')).toContainText('session-1');
});

test('Scenario 3: Valid empty agent list renders normal empty state, not connecting screen', async ({ page }) => {
  await page.goto('/?scenario=agentsEmpty');

  // Expect header
  await expect(page.locator('h2')).toHaveText('Select an Agent');

  // Normal empty state is rendered
  const noAgents = page.locator('.no-agents-msg');
  await expect(noAgents).toBeVisible();
  await expect(noAgents).toHaveText('No agents found.');

  // Connecting state must NOT be visible
  await expect(page.getByTestId('agents-connecting')).not.toBeVisible();
  await expect(page.getByTestId('agents-error')).not.toBeVisible();
});

test('Scenario 4: Agent-switch during a sessions outage shows no stale sessions from previous agent', async ({ page }) => {
  // Start on happy path and open sessions for coding/coder
  await page.goto('/?scenario=happy');
  await page.locator('.grid-item').filter({ hasText: 'coding/coder' }).click();
  await expect(page.locator('h2')).toContainText('Sessions for coding/coder');
  await expect(page.locator('.sessions-grid')).toContainText('session-1');

  // Introduce sessions 502 outage
  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.sessionsTransient502);
  });

  // Switch to researcher agent during outage without page reload
  await page.evaluate(() => {
    window.history.pushState({}, '', '/agents/researcher');
    window.dispatchEvent(new PopStateEvent('popstate'));
  });
  await expect(page.locator('h2')).toContainText('Sessions for researcher');

  // Must NOT show coding/coder's "session-1" (shows loading state instead)
  await expect(page.locator('.sessions-grid')).not.toBeVisible();

  // Shows loading indicator while retrying
  await expect(page.locator('.sessions-loading')).toBeVisible();

  // Restore happy path mid-test
  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.happy);
  });

  // Recovers automatically without reload
  await expect(page.locator('.sessions-loading')).not.toBeVisible();
});

test('Scenario 5: Network-level failure (HttpResponse.error) recovers same as 502 without reload', async ({ page }) => {
  // Start with network-level error on agents (no HTTP status, fetch rejection)
  await page.goto('/?scenario=agentsNetworkError');

  // Assert connecting state is shown
  const connecting = page.getByTestId('agents-connecting');
  await expect(connecting).toBeVisible();
  await expect(connecting).toHaveAttribute('role', 'status');

  // Flip mock handler to happy path mid-test without page reload
  await page.evaluate(() => {
    const msw = (window as any).__msw;
    msw.worker.use(...msw.scenarios.happy);
  });

  // Automatically recovers without page reload
  await expect(page.locator('h2')).toHaveText('Select an Agent');
  await expect(page.locator('.grid-item')).toContainText('coding/coder');
  await expect(connecting).not.toBeVisible();
});
