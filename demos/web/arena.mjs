// Records a scripted run of the harnx LLM Arena (?num=2).
//
// Both panels are pointed at the same mock-llm model — the script intentionally
// uses different responses across turns so the panels diverge visually.

import { withRecording, typeAndSend, DEFAULTS } from "./lib.mjs";

const argv = Object.fromEntries(
  process.argv.slice(2).reduce((acc, arg, i, arr) => {
    if (arg.startsWith("--")) acc.push([arg.slice(2), arr[i + 1]]);
    return acc;
  }, []),
);
const outDir = argv.out ?? "./out/arena";

await withRecording(outDir, async (page) => {
  await page.goto(`${DEFAULTS.serveUrl}/arena?num=2`);

  // Both <select id="model"> in the arena populate from /v1/models. Wait for
  // them to be ready, then set both panels to the mock model.
  const selects = page.locator("select#model");
  await selects.first().waitFor({ state: "visible", timeout: 15000 });
  const count = await selects.count();
  for (let i = 0; i < count; i++) {
    await selects.nth(i).selectOption("mock:mock-llm");
  }

  await typeAndSend(page, "Which of you is faster?");
  await page.waitForTimeout(4000);

  await typeAndSend(page, "Summarise harnx in one sentence.");
  await page.waitForTimeout(5000);
});
