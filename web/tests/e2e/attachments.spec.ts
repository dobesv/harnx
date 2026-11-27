import { test, expect } from '@playwright/test';

test('pending files can be previewed and removed only after confirmation', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');
  const input = page.locator('.aui-composer-input');
  await input.fill('Keep this draft');
  const chooserPromise = page.waitForEvent('filechooser');
  await page.getByRole('button', { name: 'Attach file' }).click();
  const chooser = await chooserPromise;
  await chooser.setFiles({ name: 'notes.txt', mimeType: 'text/plain', buffer: Buffer.from('Attachment preview content') });
  const pending = page.getByRole('group', { name: 'Pending attachments' });
  await expect(pending).toContainText('notes.txt');
  const listBox = await pending.boundingBox();
  const inputBox = await input.boundingBox();
  expect(listBox!.y + listBox!.height).toBeLessThanOrEqual(inputBox!.y);

  const view = page.getByRole('button', { name: 'View notes.txt' });
  await view.click();
  const dialog = page.getByRole('dialog', { name: 'notes.txt' });
  await expect(dialog).toContainText('Attachment preview content');
  const close = page.getByRole('button', { name: 'Close attachment preview' });
  await expect(close).toBeFocused();
  await page.keyboard.press('Tab');
  await expect(dialog.getByRole('button', { name: 'Remove notes.txt' })).toBeFocused();
  await page.keyboard.press('Shift+Tab');
  await expect(close).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(dialog).toBeHidden();
  await expect(view).toBeFocused();

  page.once('dialog', (confirmation) => confirmation.dismiss());
  await page.getByRole('button', { name: 'Remove notes.txt' }).click();
  await expect(pending).toContainText('notes.txt');
  await view.click();
  page.once('dialog', (confirmation) => confirmation.accept());
  await dialog.getByRole('button', { name: 'Remove notes.txt' }).click();
  await expect(dialog).toBeHidden();
  await expect(pending).not.toContainText('notes.txt');
  await expect(input).toHaveValue('Keep this draft');
});

test('previewing an image leaves it attached for sending', async ({ page }) => {
  await page.goto('/agents/coding%2Fcoder/sessions/session-1?scenario=happy');
  const chooserPromise = page.waitForEvent('filechooser');
  await page.getByRole('button', { name: 'Attach file' }).click();
  await (await chooserPromise).setFiles({
    name: 'pixel.png', mimeType: 'image/png',
    buffer: Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aX1sAAAAASUVORK5CYII=', 'base64'),
  });
  await page.getByRole('button', { name: 'View pixel.png' }).click();
  const preview = page.getByRole('dialog').getByRole('img', { name: 'pixel.png' });
  await expect(preview).toBeVisible();
  await expect(preview).toHaveJSProperty('naturalWidth', 1);
  await page.keyboard.press('Escape');
  await expect(page.getByRole('group', { name: 'Pending attachments' })).toContainText('pixel.png');
});
