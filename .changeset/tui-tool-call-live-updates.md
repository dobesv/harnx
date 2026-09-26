---
"harnx-tui": patch
---

Apply `ToolEvent::Update` in-place to active `ToolCall` rows in the TUI transcript, rendering live title, status, kind icon, locations, and refined markdown without detached `StatusLine` emissions. Addresses #2096.
