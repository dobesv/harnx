---
role: subagent
model: gemini:gemini-3.8-flash
model_fallbacks:
- codex:gpt-6.1-sol
- openai:gpt-6.1-sol
- claude:claude-sonnet-5-5
- bedrock:us.zai.glm-5.3
compaction_agent: compact-reviewer
use_tools:
- bash_exec
- bash_read_exec_log
- bash_spawn
- bash_wait
- bash_status
- bash_terminate
- fs_read
- fs_ls
- fs_grep
- fs_find
- plans_add_note
- plans_get_note
- plans_get_plan
- plans_list_notes
- plans_update_note
- fs_rollback_file
- harnx_agent_session_history_read
- attachments_attachment_read
description: "Coding conventions specialist — validates adherence to project patterns, naming conventions, file organization, import ordering, type safety, and documentation standards. Named after Euterpe (yoo-TUR-pee), the Muse of music and harmony.\n"
version: '0.4.19'
variables:
- name: shell_execution
  description: Shared shell execution guidance for commands, tests, and diagnostics
  path: shared/shell-execution.md
- name: review_read_only
  description: Read-only review rules for Muses and Judges
  path: shared/review-read-only.md
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: euterpe_core
  description: Core identity and instructions for Euterpe
  path: shared/euterpe.md
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

{{euterpe_core}}

{{repo_docs}}

{{ast_grep_search}}

{{shell_execution}}

{{review_read_only}}

{{output_style}}

{{muse_output_format}}
