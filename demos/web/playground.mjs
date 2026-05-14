// Records a scripted run of the harnx LLM Playground.
//
// Expects:
//   - harnx --serve running on $HARNX_SERVE_URL (default :8000)
//   - harnx-mock-llm running on :3829 (configured in HARNX_CONFIG_DIR)
//
// Output: writes a WebM into the directory passed via --out (the orchestrator
// then converts it to GIF with ffmpeg).

import { withRecording, selectModel, typeAndSend, DEFAULTS } from "./lib.mjs";

const argv = Object.fromEntries(
  process.argv.slice(2).reduce((acc, arg, i, arr) => {
    if (arg.startsWith("--")) acc.push([arg.slice(2), arr[i + 1]]);
    return acc;
  }, []),
);
const outDir = argv.out ?? "./out/playground";

await withRecording(outDir, async (page) => {
  await page.goto(`${DEFAULTS.serveUrl}/playground`);
  await selectModel(page, "mock:mock-llm");

  await typeAndSend(page, "Explain what harnx is in one paragraph.");
  // Wait for the assistant bubble to settle.
  await page.waitForTimeout(4000);

  await typeAndSend(page, "Now show me a small code example.");
  await page.waitForTimeout(5000);
});
