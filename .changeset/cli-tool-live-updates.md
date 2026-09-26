---
harnx: patch
---

CLI streamed tool progress notices

Handle `ToolEvent::Update` in the CLI event sink to print concise, dimmed notices when a tool's title changes during execution. Updates are rate-limited (max once every 2 seconds) and deduplicated to prevent terminal flooding. Quiet for tools that don't emit updates. Existing completion and error behavior is preserved.

Closes #2096
