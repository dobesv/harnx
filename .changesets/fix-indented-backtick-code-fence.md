---
harnx: patch
---
Fix markdown renderer incorrectly treating 4+-space-indented triple-backtick lines as code fences.

Per the CommonMark spec, a fenced code block opener must have 0–3 spaces of indentation.
Lines with 4+ leading spaces (e.g. indented Python code in a bash command) were previously
being recognised as code block delimiters, causing the TUI to render subsequent lines as
highlighted code instead of plain text. Fixes #403.
