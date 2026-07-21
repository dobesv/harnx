---
role: subagent
model: gemini:gemini-3.6-flash
compaction_agent: compact-argus
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
- plans_get_task
- plans_list_notes
- plans_list_tasks
- plans_update_note
- plans_update_task
- harnx_agent_session_history_read
description: "Task verification agent — independently verifies work completed by other agents by reading changed files, running tests and diagnostics, and cross-checking claims against actual results. Returns structured PASS/FAIL verdicts with evidence. Named after Argus (AR-gus) Panoptes, the hundred-eyed giant who never slept.\n"
version: '0.3.3'
variables:
- name: argus_core
  description: Core verification protocol for argus
  path: shared/argus.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{argus_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
