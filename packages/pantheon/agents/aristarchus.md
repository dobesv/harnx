---
role: assistant
model: claude:claude-sonnet-5-5
model_fallbacks:
- codex:gpt-6.1-sol
- openai:gpt-6.1-sol
- gemini:gemini-3.8-flash
- bedrock:us.zai.glm-5.3
compaction_agent: compact-reviewer
use_tools:
  - aeacus_session_prompt
  - aeacus_session_send
  - aeacus_session_wait
  - aeacus_session_load

  - bash_exec
  - bash_read_exec_log
  - bash_spawn
  - bash_wait
  - bash_terminate
  - calliope_session_prompt
  - calliope_session_send
  - calliope_session_wait
  - calliope_session_load

  - erato_session_prompt
  - erato_session_send
  - erato_session_wait
  - erato_session_load

  - euterpe_session_prompt
  - euterpe_session_send
  - euterpe_session_wait
  - euterpe_session_load

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
  - librarian_session_prompt
  - librarian_session_send
  - librarian_session_wait
  - librarian_session_load

  - melpomene_session_prompt
  - melpomene_session_send
  - melpomene_session_wait
  - melpomene_session_load

  - minos_session_prompt
  - minos_session_send
  - minos_session_wait
  - minos_session_load

  - nemesis_session_prompt
  - nemesis_session_send
  - nemesis_session_wait
  - nemesis_session_load

  - opis_session_prompt
  - opis_session_send
  - opis_session_wait
  - opis_session_load

  - oracle_session_prompt
  - oracle_session_send
  - oracle_session_wait
  - oracle_session_load

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
  - polyhymnia_session_send
  - polyhymnia_session_wait
  - polyhymnia_session_load

  - pytheas_session_prompt
  - pytheas_session_send
  - pytheas_session_wait
  - pytheas_session_load

  - rhadamanthus_session_prompt
  - rhadamanthus_session_send
  - rhadamanthus_session_wait
  - rhadamanthus_session_load

  - terpsichore_session_prompt
  - terpsichore_session_send
  - terpsichore_session_wait
  - terpsichore_session_load

  - thalia_session_prompt
  - thalia_session_send
  - thalia_session_wait
  - thalia_session_load

  - tyche_session_prompt
  - tyche_session_send
  - tyche_session_wait
  - tyche_session_load

  - urania_session_prompt
  - urania_session_send
  - urania_session_wait
  - urania_session_load

  - harnx_agent_session_history_read
  - attachments_attachment_read
  - attachments_attachment_create
description: "Code review coordinator \u2014 orchestrates multi-agent code review\
  \ of pull requests and codebases, aggregating specialist findings into structured\
  \ verdicts. Named after Aristarchus (ar-ih-STAR-kus) of Samothrace, the greatest\
  \ textual critic of antiquity.\n"
version: '0.4.19'
variables:
- name: shell_execution
  description: Shared shell execution guidance for commands, tests, and diagnostics
  path: shared/shell-execution.md
- name: async_delegation
  description: Preferred async delegation protocol (session_send/session_wait)
  path: shared/async-delegation.md
- name: aristarchus_core
  description: Core identity and instructions for Aristarchus
  path: shared/aristarchus.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: natural_writing
  description: Natural writing style rules for prose (comments, docs, commits, PRs)
  path: shared/natural-writing.md
- name: policy_test_coverage
  description: Test coverage policy — trigger, exemptions, opt-out rules
  path: shared/policy-test-coverage.md
---

{{aristarchus_core}}

{{async_delegation}}

{{policy_test_coverage}}

{{natural_writing}}

{{shell_execution}}

{{output_style}}
