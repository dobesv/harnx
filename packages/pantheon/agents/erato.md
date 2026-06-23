---
role: subagent
model: gemini:gemini-3-flash-preview
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
- plans_add_note
- plans_get_note
- plans_get_plan
- plans_list_notes
- plans_update_note
- fs_rollback_file
- harnx_agent_session_history_read
description: "UI/UX and accessibility specialist — evaluates design system compliance, responsive design, WCAG accessibility, ARIA usage, keyboard navigation, and user experience patterns. Named after Erato (EH-ruh-toh), the Muse of love poetry — ensuring the UI is lovable and accessible to all.\n"
version: '0.3.1'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: erato_core
  description: Core identity and instructions for Erato
  path: shared/erato.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{erato_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
