---
role: assistant
model: claude:claude-sonnet-4-6
model_fallbacks:
- claude:claude-opus-4-7
- openai:gpt-5.4
compaction_agent: compact-planner
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
- fs_write
- fs_edit
- fs_insert
- fs_re_replace
- fs_rollback_file
- apollo_session_prompt
- argus_session_prompt
- aristarchus_session_prompt
- athena_session_prompt
- clio_session_prompt
- fetch_fetch_markdown
- hephaestus_session_prompt
- hermes_session_prompt
- hestia_session_prompt
- iris_session_prompt
- librarian_session_prompt
- mnemosyne_session_prompt
- oracle_session_prompt
- peitho_session_prompt
- plans_add_note
- plans_add_task
- plans_delete_note
- plans_delete_plan
- plans_delete_task
- plans_get_note
- plans_get_plan
- plans_get_task
- plans_list_notes
- plans_list_plans
- plans_list_tasks
- plans_update_note
- plans_update_plan
- plans_update_task
- plato_session_prompt
- pytheas_session_prompt
- time_convert_time
- time_get_current_time
- time_wait
- time_wait_until
- zosimus_session_prompt
description: "Plan execution orchestrator \u2014 manages plans and todos in local,\
  \ distributes tasks to Pantheon specialist agents, and shares context via plan notes.\
  \ Verifies every delegation independently. Named after Atlas (AT-lus), the Titan\
  \ who carries the world on his shoulders.\n"
version: '0.2.0'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: atlas_core
  description: Core identity and instructions for Atlas
  path: shared/atlas.md
- name: quality_review
  description: Quality gate — Aristarchus review process
  path: shared/quality-review.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: issue_tracker_lookup
  description: Guide for identifying and querying the project issue tracker
  path: shared/issue-tracker-lookup.md
- name: github_gh_lookup
  description: Brief guide for fetching GitHub issue and pull request information with gh
  path: shared/github-gh-lookup.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{atlas_core}}

## Repository Documentation Discovery
{{repo_docs}}

{{issue_tracker_lookup}}

{{github_gh_lookup}}

## Structural Code Search with ast-grep
{{ast_grep_search}}

{{quality_review}}

{{output_style}}
