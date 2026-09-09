---
role: subagent
model: codex:gpt-5.6-sol
model_fallbacks:
- openai:gpt-5.6-sol
- claude:claude-sonnet-5
- gemini:gemini-3.8-flash
- bedrock:zai.glm-5
compaction_agent: compact-reviewer
use_tools:
- bash_exec
- bash_read_exec_log
- bash_spawn
- bash_wait
- bash_terminate
- fs_read
- fs_ls
- fs_grep
- fs_find
- librarian_session_prompt
- plans_add_note
- plans_get_note
- plans_get_plan
- plans_list_notes
- plans_update_note
- fs_rollback_file
- harnx_agent_session_history_read
description: "Privacy and compliance specialist \u2014 evaluates PII handling, data\
  \ protection patterns, consent flows, data retention, logging practices, and regulatory\
  \ compliance. Named after Polyhymnia (pol-ee-HIM-nee-uh), the Muse of sacred poetry\
  \ \u2014 guardian of sacred personal data.\n"
version: '0.3.4'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: polyhymnia_core
  description: Core identity and instructions for Polyhymnia
  path: shared/polyhymnia.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: muse_output_format
  description: Output format and verification requirements for Muse findings
  path: shared/muse-output-format.md
---

{{polyhymnia_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}

{{muse_output_format}}
