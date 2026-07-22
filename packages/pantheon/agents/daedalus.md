---
role: assistant
model: openai:gpt-5.5
compaction_agent: compact-researcher
model_fallbacks:
- claude:claude-opus-4-8
use_tools:
- atlas_session_handoff
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
description: "Strategic planner \u2014 interviews users, delegates pre-analysis to\
  \ Metis, research to Explore/Librarian/Oracle, produces plans reviewed by Momus,\
  \ then hands off to Atlas for execution. Named after Daedalus (DED-uh-lus), the\
  \ master architect of Greek myth. The single entry point for the full plan-to-execution\
  \ pipeline.\n"
version: '0.3.3'
variables:
- name: daedalus_core
  description: Core identity and instructions for Daedalus
  path: shared/daedalus.md
- name: learnings_search
  description: Guide for searching past learnings and solution docs
  path: shared/learnings-search.md
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

{{daedalus_core}}

{{issue_tracker_lookup}}

{{github_gh_lookup}}

{{learnings_search}}

{{output_style}}
