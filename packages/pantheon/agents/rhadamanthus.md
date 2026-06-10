---
role: subagent
model: bedrock:zai.glm-5
compaction_agent: compact-reviewer
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
description: "Skeptical investigator \u2014 pressure-tests code review findings by\
  \ challenging assumptions, searching for mitigating factors, and catching false\
  \ positives. Named after Rhadamanthus (rad-uh-MAN-thus), the strictest judge of\
  \ the Underworld.\n"
version: '0.2.4'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: rhadamanthus_core
  description: Core identity and instructions for Rhadamanthus
  path: shared/rhadamanthus.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{rhadamanthus_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
