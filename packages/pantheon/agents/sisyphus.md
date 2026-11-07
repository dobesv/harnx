---
role: assistant
model: claude:claude-opus-5-5
model_fallbacks:
- codex:gpt-6.1-sol
- openai:gpt-6.1-sol
- gemini:gemini-4-argon
- bedrock:us.moonshotai.kimi-k3
compaction_agent: compact-dev
use_tools:
- bash_exec
- bash_read_exec_log
- bash_spawn
- bash_wait
- bash_terminate
- bash_wait_for_pr_stable
- fs_read
- fs_ls
- fs_grep
- fs_find
- fs_write
- fs_write_attachment
- fs_edit
- fs_insert
- fs_re_replace
- fs_rollback_file
- apollo_session_prompt
- apollo_session_send
- apollo_session_wait
- apollo_session_load

- argus_session_prompt
- argus_session_send
- argus_session_wait
- argus_session_load

- aristarchus_session_prompt
- aristarchus_session_send
- aristarchus_session_wait
- aristarchus_session_load

- athena_session_prompt
- athena_session_send
- athena_session_wait
- athena_session_load

- clio_session_prompt
- clio_session_send
- clio_session_wait
- clio_session_load

- fetch_fetch_markdown
- hephaestus_session_prompt
- hephaestus_session_send
- hephaestus_session_wait
- hephaestus_session_load

- hermes_session_prompt
- hermes_session_send
- hermes_session_wait
- hermes_session_load

- hestia_session_prompt
- hestia_session_send
- hestia_session_wait
- hestia_session_load

- iris_session_prompt
- iris_session_send
- iris_session_wait
- iris_session_load

- librarian_session_prompt
- librarian_session_send
- librarian_session_wait
- librarian_session_load

- mnemosyne_session_prompt
- mnemosyne_session_send
- mnemosyne_session_wait
- mnemosyne_session_load

- oracle_session_prompt
- oracle_session_send
- oracle_session_wait
- oracle_session_load

- peitho_session_prompt
- peitho_session_send
- peitho_session_wait
- peitho_session_load

- plans_add_note
- plans_add_plan
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
- plato_session_send
- plato_session_wait
- plato_session_load

- pytheas_session_prompt
- pytheas_session_send
- pytheas_session_wait
- pytheas_session_load

- time_convert_time
- time_get_current_time
- time_wait
- time_wait_until
- zosimus_session_prompt
- zosimus_session_send
- zosimus_session_wait
- zosimus_session_load

- harnx_agent_session_history_read
- attachments_attachment_read
- attachments_attachment_create
description: "Task executor \u2014 handles tasks from users. For complex work,\
  \ creates local plans and executes them directly or delegates to Pantheon specialists.\
  \ Writes code directly in the project directory. Named after Sisyphus (SIS-ih-fus).\n"
version: '0.4.19'
variables:
- name: async_delegation
  description: Preferred async delegation protocol (session_send/session_wait)
  path: shared/async-delegation.md
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
- name: learnings_search
  description: Required protocol for researching repository context and verified history
  path: shared/learnings-search.md
- name: github_gh_lookup
  description: Brief guide for fetching GitHub issue and pull request information with gh
  path: shared/github-gh-lookup.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: natural_writing
  description: Natural writing style rules for prose (comments, docs, commits, PRs)
  path: shared/natural-writing.md
---

{{sisyphus_core}}

{{async_delegation}}

{{issue_tracker_lookup}}

{{github_gh_lookup}}

{{repo_docs}}

{{learnings_search}}

{{ast_grep_search}}

{{ast_grep_rewrite}}

{{sisyphus_jira_ask}}

{{sisyphus_resuming_prs}}

{{sisyphus_clio_delegation}}

{{quality_review}}

{{sisyphus_default_repos}}

{{sisyphus_plan_notes}}

{{natural_writing}}

{{output_style}}
