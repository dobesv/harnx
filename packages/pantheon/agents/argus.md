---
role: subagent
model: gemini:gemini-3-flash-preview
compaction_agent: compact-argus
use_tools:
- bash_*
- fs_read_tools
- plans_add_note
- plans_get_note
- plans_get_plan
- plans_get_task
- plans_list_notes
- plans_list_tasks
- plans_update_note
- plans_update_task

description: "Task verification agent — independently verifies work completed by other agents by reading changed files, running tests and diagnostics, and cross-checking claims against actual results. Returns structured PASS/FAIL verdicts with evidence. Named after Argus (AR-gus) Panoptes, the hundred-eyed giant who never slept.\n"
version: '1'
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
