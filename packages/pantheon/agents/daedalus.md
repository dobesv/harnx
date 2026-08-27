---
role: assistant
model: claude:claude-opus-4-8
compaction_agent: compact-researcher
model_fallbacks:
- gemini:gemini-3.1-pro-preview
- openai:gpt-5.6-sol
use_tools:
- atlas_session_handoff
- bash_gh_issue_list
- bash_gh_issue_view
- bash_gh_pr_comments
- bash_gh_pr_files
- bash_gh_pr_view
- bash_jira_search
- bash_jira_view
- librarian_session_prompt
- metis_session_prompt
- momus_session_prompt
- oracle_session_prompt
- plans_add_note
- plans_add_task
- plans_delete_note
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
- pytheas_session_prompt
- zosimus_session_prompt
- harnx_agent_session_history_read
hooks:
  entries:
  - command: >-
      harnx-claude-compatible-hook-server
      --event PreToolUse
      --matcher '^atlas_session_handoff$'
      --jaq
      '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Hand off this plan to Atlas for execution?"}}'
description: "Strategic planner \u2014 interviews users, delegates pre-analysis to\
  \ Metis, research to Explore/Librarian/Oracle, produces plans reviewed by Momus,\
  \ then hands off to Atlas for execution. Named after Daedalus (DED-uh-lus), the\
  \ master architect of Greek myth. The single entry point for the full plan-to-execution\
  \ pipeline.\n"
version: '0.3.4'
variables:
- name: daedalus_core
  description: Core identity and instructions for Daedalus
  path: shared/daedalus.md
- name: daedalus_lookup_tools
  description: Scoped tools for reading GitHub issues/PRs and Jira (no shell)
  path: shared/daedalus-lookup-tools.md
- name: learnings_search
  description: Required protocol for researching repository context and verified history
  path: shared/learnings-search.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: natural_writing
  description: Natural writing style rules for prose (comments, docs, commits, PRs)
  path: shared/natural-writing.md
---

{{daedalus_core}}

{{daedalus_lookup_tools}}

{{learnings_search}}

{{natural_writing}}

{{output_style}}
