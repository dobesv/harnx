---
role: subagent
model: codex:gpt-6.1-sol
model_fallbacks:
- openai:gpt-6.1-sol
- claude:claude-sonnet-5-5
- gemini:gemini-4-argon
- bedrock:us.moonshotai.kimi-k3
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
- librarian_session_send
- librarian_session_wait
- librarian_session_load

- plans_add_note
- plans_get_note
- plans_get_plan
- plans_list_notes
- plans_update_note
- fs_rollback_file
- harnx_agent_session_history_read
- attachments_attachment_read
description: "Privacy and compliance specialist \u2014 evaluates PII handling, data\
  \ protection patterns, consent flows, data retention, logging practices, and regulatory\
  \ compliance. Named after Polyhymnia (pol-ee-HIM-nee-uh), the Muse of sacred poetry\
  \ \u2014 guardian of sacred personal data.\n"
version: '0.4.19'
variables:
- name: shell_execution
  description: Shared shell execution guidance for commands, tests, and diagnostics
  path: shared/shell-execution.md
- name: review_read_only
  description: Read-only review rules for Muses and Judges
  path: shared/review-read-only.md
- name: async_delegation
  description: Preferred async delegation protocol (session_send/session_wait)
  path: shared/async-delegation.md
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

{{async_delegation}}

{{repo_docs}}

{{ast_grep_search}}

{{shell_execution}}

{{review_read_only}}

{{output_style}}

{{muse_output_format}}
