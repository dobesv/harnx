---
harnx: patch
---
Render inline markdown in MCP tool-template output for both TUI and CLI.

The built-in tool templates use Markdown like `**$** \`{{ args.command }}\``
to make the rendered call/result lines scannable. PR #386 wired the
rendered text into the transcript but displayed it verbatim, so the
markers leaked through as literal asterisks and backticks instead of
producing styling.

- TUI: new `ToolCallBody::Markdown` and `TranscriptItem::ToolResultMarkdown`
  variants route templated text through `tui-markdown`, which produces
  ratatui spans with `BOLD` / `ITALIC` modifiers and a styled fg/bg for
  inline `` `code` `` runs. Raw YAML/output bodies still render plain so
  YAML keys/values are never accidentally styled.
- CLI: templated `Started` titles and templated `Completed` lines run
  through the existing `MarkdownRender` (syntect ANSI). Raw output keeps
  the dim plain-text path. Markdown rendering is bypassed when the renderer
  can't initialize, when `--no-highlight` is set, or when stdout isn't a
  TTY.

`AssistantText` is also wired through the same renderer so headings,
lists, code fences, and inline emphasis from the LLM display correctly
(this rendering was lost in the original ratatui transition). Single
input newlines are preserved as visible line breaks rather than being
collapsed into reflowed paragraphs the way CommonMark normally would.

Other transcript items (`Plan`, `SystemText`, `ErrorText`, etc.) still
render plain — they're not user-content and don't benefit from
markdown.
