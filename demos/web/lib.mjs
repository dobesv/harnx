// Shared helpers for the web-demo Playwright scripts.

import { chromium } from "playwright";
import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";

export const DEFAULTS = {
  serveUrl: process.env.HARNX_SERVE_URL ?? "http://127.0.0.1:8000",
  viewport: { width: 1280, height: 800 },
};

export async function withRecording(outDir, fn) {
  const absOut = resolve(outDir);
  mkdirSync(absOut, { recursive: true });

  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    viewport: DEFAULTS.viewport,
    recordVideo: { dir: absOut, size: DEFAULTS.viewport },
  });
  const page = await context.newPage();

  try {
    await fn(page);
  } finally {
    await page.close();
    await context.close();
    await browser.close();
  }
}

export async function selectModel(page, modelId) {
  // The playground/arena populate the <select id="model"> from /v1/models on
  // load. Wait for the option to appear before changing it.
  const sel = page.locator("select#model").first();
  await sel.waitFor({ state: "visible", timeout: 15000 });
  // <option> elements inside <select> are reported as hidden by Playwright's
  // default visibility heuristic; wait for "attached" instead.
  await sel
    .locator(`option[value="${modelId}"]`)
    .waitFor({ state: "attached", timeout: 15000 });
  await sel.selectOption(modelId);
}

export async function typeAndSend(page, text, { perChar = 25 } = {}) {
  const input = page.locator("textarea#chat-input").first();
  await input.click();
  await input.type(text, { delay: perChar });
  await page.keyboard.press("Enter");
}
