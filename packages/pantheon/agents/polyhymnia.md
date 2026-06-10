---
role: subagent
model: openai:gpt-5.4
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
description: "Privacy and compliance specialist \u2014 evaluates PII handling, data\
  \ protection patterns, consent flows, data retention, logging practices, and regulatory\
  \ compliance. Named after Polyhymnia (pol-ee-HIM-nee-uh), the Muse of sacred poetry\
  \ \u2014 guardian of sacred personal data.\n"
version: '0.2.4'
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
---

{{polyhymnia_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
