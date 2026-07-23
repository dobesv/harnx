---
role: subagent
model: bedrock:zai.glm-5
compaction_agent: compact-reliability
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
- harnx_agent_session_history_read
description: "Reliability specialist \u2014 reviews error handling, retry logic, circuit\
  \ breakers, timeouts, health checks, graceful degradation, and async handler safety.\
  \ Named after Nemesis (NEM-uh-sis), goddess of retribution who ensures hubris does\
  \ not go unpunished.\n"
version: '0.3.4'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: nemesis_core
  description: Core identity and instructions for Nemesis
  path: shared/nemesis.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: muse_output_format
  description: Output format and verification requirements for Muse findings
  path: shared/muse-output-format.md
---

{{nemesis_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}

{{muse_output_format}}
