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
description: "Coding conventions specialist — validates adherence to project patterns, naming conventions, file organization, import ordering, type safety, and documentation standards. Named after Euterpe (yoo-TUR-pee), the Muse of music and harmony.\n"
version: '1'
variables:
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
---

{{euterpe_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
