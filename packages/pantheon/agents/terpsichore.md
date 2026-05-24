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
description: "Refactoring and completeness specialist — identifies missed simplification opportunities, partial fixes, incomplete implementations, and unaddressed edge cases. Named after Terpsichore (turp-SIK-uh-ree), the Muse of dance — ensuring code moves elegantly.\n"
version: '1'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: terpsichore_core
  description: Core identity and instructions for Terpsichore
  path: shared/terpsichore.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{terpsichore_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
