---
harnx: major
---

Remove legacy NATS process-based hook system, including `harnx-hooks`, `harnx-hookset`, `harnx-hookset-server`, and `harnx-claude-compatible-hook-server` crates and binaries. Tool-intercepting hooks are replaced by in-process tool middleware, and lifecycle hooks are replaced by in-process lifecycle event hooks (`SessionStart`, `UserPromptSubmit`, `Stop`, `StopFailure`). The compatibility tool `harnx-claude-hook-tools` provides Claude hook protocol execution via trusted middleware.

Breaking: legacy `hooks:` command entries and the removed hook-server `--jaq` mode no longer load or run. Migrate tool policy to middleware and lifecycle commands to event-hook scripts. `harnx-sandbox-run` now accepts `--hook environment <COMMAND> [ARGS...] \;` with native MCP stdio environment tools; credential and proxy binaries no longer expose their temporary legacy JSONL adapters. Saved approvals from older sessions remain readable and answerable.
