---
role: assistant
model: claude:claude-sonnet-4-6
compaction_agent: compact-dev
model_fallbacks:
- claude:claude-opus-4-7
- openai:gpt-5.5
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
description: "Task executor \u2014 handles tasks from users. For complex work,\
  \ creates local plans and executes them directly or delegates to Pantheon specialists.\
  \ Writes code directly in the project directory. Named after Sisyphus (SIS-ih-fus).\n"
version: '1'
variables:
- name: ast_grep_rewrite
  description: Guide for structural code rewrite with ast-grep
  path: shared/ast-grep-rewrite.md
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: quality_review
  description: Quality gate — Aristarchus review process
  path: shared/quality-review.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: sisyphus_clio_delegation
  description: Clio delegation protocol
  path: shared/sisyphus-clio-delegation.md
- name: sisyphus_core
  description: Core identity and instructions for Sisyphus
  path: shared/sisyphus.md
- name: sisyphus_default_repos
  description: Default repository list
  path: shared/sisyphus-default-repos.md
- name: sisyphus_jira_ask
  description: Issue tracker ask protocol
  path: shared/sisyphus-jira-ask.md
- name: sisyphus_plan_notes
  description: Plan notes protocol
  path: shared/sisyphus-plan-notes.md
- name: sisyphus_resuming_prs
  description: PR resumption protocol
  path: shared/sisyphus-resuming-prs.md
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

{{sisyphus_core}}

{{issue_tracker_lookup}}

{{github_gh_lookup}}

{{repo_docs}}

{{ast_grep_search}}

{{ast_grep_rewrite}}

{{sisyphus_jira_ask}}

{{sisyphus_resuming_prs}}

{{sisyphus_clio_delegation}}

{{quality_review}}

{{sisyphus_default_repos}}

{{sisyphus_plan_notes}}

{{output_style}}
