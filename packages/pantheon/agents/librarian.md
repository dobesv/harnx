---
role: subagent
model: gemini:gemini-3.8-flash
model_fallbacks:
- codex:gpt-5.6-terra
- openai:gpt-5.6-terra
- claude:claude-sonnet-5
- bedrock:zai.glm-5
compaction_agent: compact-researcher
use_tools:
- context7_query-docs
- context7_resolve-library-id
- exa_web_search_exa
- fetch_fetch_markdown
- grep_grep_query
- harnx_agent_session_history_read
description: "External knowledge researcher \u2014 searches the web, library documentation,\
  \ and public GitHub repositories to find best practices, patterns, API references,\
  \ and solutions to technical questions.\n"
version: '0.3.4'
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
