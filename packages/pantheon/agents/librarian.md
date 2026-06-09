---
role: subagent
model: gemini:gemini-3.1-pro-preview
compaction_agent: compact-researcher
use_tools:
- context7_query-docs
- context7_resolve-library-id
- exa_web_search_exa
- fetch_fetch_markdown
- grep_grep_query
description: "External knowledge researcher \u2014 searches the web, library documentation,\
  \ and public GitHub repositories to find best practices, patterns, API references,\
  \ and solutions to technical questions.\n"
version: '0.2.3'
variables:
- name: librarian_core
  description: Core identity and instructions for Librarian
  path: shared/librarian.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{librarian_core}}

{{output_style}}
