---
role: subagent
model: bedrock:zai.glm-5
compaction_agent: compact-reviewer
use_tools:
- bash_exec
- bash_*
- fs_read_tools
- oracle_session_prompt
- plans_add_note
- plans_get_note
- plans_get_plan
- plans_list_notes
- plans_update_note
- pytheas_session_prompt
- fs_rollback_file

description: "Architecture and big picture specialist \u2014 evaluates cross-cutting\
  \ concerns, dependency direction, design pattern adherence, API contract consistency,\
  \ and system-wide impact. Named after Urania (yoo-RAY-nee-uh), the Muse of astronomy\
  \ \u2014 seeing the codebase from above.\n"
version: '1'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: urania_core
  description: Core identity and instructions for Urania
  path: shared/urania.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{urania_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
