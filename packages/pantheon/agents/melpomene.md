---
role: subagent
model: openai:gpt-5.6-sol
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
- librarian_session_prompt
- oracle_session_prompt
- plans_add_note
- plans_get_note
- plans_get_plan
- plans_list_notes
- plans_update_note
- fs_rollback_file
- harnx_agent_session_history_read
description: "Security vulnerability specialist \u2014 identifies injection risks,\
  \ authentication flaws, authorization gaps, secrets exposure, input validation issues,\
  \ and supply chain risks. Named after Melpomene (mel-POM-uh-nee), the Muse of tragedy\
  \ \u2014 preventing security tragedies.\n"
version: '0.3.3'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: melpomene_core
  description: Core identity and instructions for Melpomene
  path: shared/melpomene.md
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

{{melpomene_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}

{{muse_output_format}}
