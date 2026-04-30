---
harnx: patch
---
Render history diffs as syntax-highlighted markdown in the TUI and CLI.

The unified diffs that `harnx-mcp-fs` and `harnx-mcp-bash` append to
mutating tool responses (issue #398) now arrive wrapped in a
` ```diff ` markdown fence. Both renderers route the entire tool result
text through their existing multi-line markdown path:

- TUI: `TranscriptItem::ToolResultMarkdown` is produced once per tool
  result (instead of per line) and rendered via
  `render_helpers::markdown_lines`, which delegates to `tui-markdown`
  (pulldown-cmark + syntect's bundled `Diff` syntax). Removed/added/hunk
  lines now render with distinct fg colors. The `ToolResultText` variant
  is gone — every tool result goes through the markdown path.
- CLI: `print_tool_completed` now always calls
  `MarkdownRender::render` (the multi-line, state-tracking renderer),
  matching the assistant-text path. Highlighting is bypassed when
  `--no-highlight` is set, when stdout isn't a TTY, or when the
  renderer fails to initialize, in which case we fall back to
  `dimmed_text`.

The diff producer (`harnx-mcp-history`) emits the `commit <sha>` /
title header as plain text above the fence so the assistant can still
grep it out for `rollback_file`. Truncation markers continue to live
inside the fence.
