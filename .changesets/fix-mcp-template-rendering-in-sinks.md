---
harnx: patch
---
Fix MCP MiniJinja templates not appearing in TUI or CLI output.

PR #349 wired template rendering into `ToolEvent::Started.title` and the
`Completed` event, but every consumer discarded those fields — so configured
templates were rendered into events that nobody read. This PR makes them
actually display:

- TUI `Started` handler now uses the rendered title instead of yaml-formatting
  the raw input.
- TUI `Completed` rendering uses the rendered template when present, falls
  back to the historic extract+truncate behavior otherwise.
- CLI `Started` line now appends the rendered title after the tool name.
- CLI `Completed` is no longer silent — renders the templated form when
  present, or the extracted/truncated raw output otherwise. (Restores the
  pre-`0daecac` CLI behavior; the silent default introduced when the sink
  architecture landed was a regression.)

Renamed `ToolEvent::Completed.content: Vec<ContentBlock>` → `title:
Option<String>` to mirror `Started.title`/`Update.title` and reflect the
single-string display semantics. Display text is producer-side render output
only; `output: Value` continues to carry the raw tool result that the LLM
sees and that gets persisted.

Extracted a shared `render_tool_result_text` helper into
`harnx_runtime::utils` so the TUI and CLI sinks format identically.

Test changes:
- New TUI tests verify the templated title and fallback rendering paths for
  both `ToolEvent::Started` and `ToolEvent::Completed` (templated `title`
  honored, raw input/output used when `title` is `None`).
- New CLI tests verify the helper covers the template-present, MCP content
  extraction, raw String, JSON YAML fallback, and empty-title paths.
- New producer-side tests in `harnx-runtime` verify that the emit functions
  populate the title fields when a matching declaration carries a template.
- Fixed a pre-existing flaky test in `harnx-core::sink` (two tests racing on
  the global sink without locking) by adding a poison-safe module-level mutex.

Session restoration still re-renders saved tool calls/results without
templates because `lifecycle.rs` doesn't have access to the live tool
declaration map at replay time. Tracked in #385.
