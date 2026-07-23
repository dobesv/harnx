---
role: subagent
model: bedrock:zai.glm-5
compaction_agent: compact-deploy
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
description: "Deployment verification specialist \u2014 produces Go/No-Go deployment\
  \ checklists with pre-deployment checks, migration verification, rollback procedures,\
  \ and monitoring plans. Named after Tyche (TY-kee), goddess of fortune who determines\
  \ whether chance favors the prepared.\n"
version: '0.3.4'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: tyche_core
  description: Core identity and instructions for Tyche
  path: shared/tyche.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: muse_output_format
  description: Output format and verification requirements for Muse findings
  path: shared/muse-output-format.md
---

{{tyche_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}

{{muse_output_format}}
