---
model: openai:gpt-5.4
compaction_agent: compact-reviewer
use_tools:
  - aeacus_session_prompt
  - bash_exec
  - bash_read_exec_log
  - bash_spawn
  - bash_wait
  - bash_terminate
  - calliope_session_prompt
  - erato_session_prompt
  - euterpe_session_prompt
  - fs_read
  - fs_ls
  - fs_grep
  - fs_find
  - fs_write
  - fs_edit
  - fs_insert
  - fs_re_replace
  - fs_rollback_file
  - librarian_session_prompt
  - melpomene_session_prompt
  - minos_session_prompt
  - nemesis_session_prompt
  - oracle_session_prompt
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
  - plans_list_tasks
  - plans_update_note
  - plans_update_plan
  - plans_update_task
  - polyhymnia_session_prompt
  - pytheas_session_prompt
  - rhadamanthus_session_prompt
  - terpsichore_session_prompt
  - thalia_session_prompt
  - tyche_session_prompt
  - urania_session_prompt
  - harnx_agent_session_history_read
description: "Code review coordinator \u2014 orchestrates multi-agent code review\
  \ of pull requests and codebases, aggregating specialist findings into structured\
  \ verdicts. Named after Aristarchus (ar-ih-STAR-kus) of Samothrace, the greatest\
  \ textual critic of antiquity.\n"
version: '0.3.2'
variables:
- name: aristarchus_core
  description: Core identity and instructions for Aristarchus
  path: shared/aristarchus.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: policy_test_coverage
  description: Test coverage policy — trigger, exemptions, opt-out rules
  path: shared/policy-test-coverage.md
---

{{aristarchus_core}}

{{policy_test_coverage}}

{{output_style}}
