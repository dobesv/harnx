---
role: subagent
model: gemini:gemini-3-flash-preview
compaction_agent: compact-reviewer
use_tools:
- bash_exec
- bash_*
- fs_read_tools
- plans_add_note
- plans_get_note
- plans_get_plan
- plans_list_notes
- plans_update_note
- fs_rollback_file

description: "Code quality specialist — identifies code smells, DRY violations, complexity issues, naming problems, and SOLID principle adherence. Named after Calliope (kuh-LY-uh-pee), the Muse of epic poetry and eloquence.\n"
version: '1'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: calliope_core
  description: Core identity and instructions for Calliope
  path: shared/calliope.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{calliope_core}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}
