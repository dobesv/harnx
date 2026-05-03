---
harnx: patch
---
Edit and other mutating filesystem/bash tools no longer silently drop their
unified-diff content block when harnx-mcp-fs / harnx-mcp-bash receive their
roots via the MCP `roots/list` protocol rather than `--root` CLI arguments
(the production launch path). `HistoryManager` now lazily discovers and
tracks the git repo containing each touched path at snapshot time instead of
freezing the tracked-repo set at construction.
