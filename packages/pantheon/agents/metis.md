---
role: subagent
model: bedrock:zai.glm-5
compaction_agent: compact-planner
use_tools:
- bash_exec
- bash_*
- fs_read_tools
- librarian_session_prompt
- pytheas_session_prompt
- fs_rollback_file

description: "Pre-planning consultant \u2014 analyzes user requests before Daedalus\
  \ generates plans, identifying hidden intentions, ambiguities, and AI failure points.\
  \ Produces directives that guide the planner. Named after Metis (MEE-tis), the Greek\
  \ goddess of wisdom, prudence, and deep counsel.\n"
version: '1'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: learnings_search
  description: Guide for searching past learnings and solution docs
  path: shared/learnings-search.md
- name: metis_core
  description: Core identity and instructions for Metis
  path: shared/metis.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
---

{{metis_core}}

{{repo_docs}}

{{ast_grep_search}}

{{learnings_search}}

{{output_style}}
