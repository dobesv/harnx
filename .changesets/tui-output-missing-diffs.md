---
harnx: patch
---
Show history diffs in tool output (closes #398).

PR #353 made `harnx-mcp-fs` and `harnx-mcp-bash` append a unified diff
as an extra content block on every mutating tool response, but the
existing `_meta.result_template` was hardcoded to
`{{ result.content[0].text | default('') }}`. That template only
rendered the first content block (the summary) and silently dropped the
diff that came after it, so edits looked like
`Edited /path (1 replacement)` with no visible patch.

The templates were never doing anything that the MCP client's generic
audience-aware renderer (`extract_user_display_text`) couldn't do better
— it already concatenates every user-audience and unaudienced content
block. We now drop `result_template` entirely from the built-in
`harnx-mcp-fs` and `harnx-mcp-bash` tool metas and let the client fall
back to that path. As a side effect, `read` / `grep` / `find` / `ls`
and bash `exec` / `spawn` / `wait` now show the user-audience summary
in the TUI instead of the assistant-only full output preview, matching
the audience annotations these tools already produced.

`call_template` is unchanged — the rendered call header
(`**edit** \`{{ args.path }}\``, `**$** \`{{ args.command }}\``, etc.)
still styles the tool-call line.
