---
role: subagent
model: bedrock:us.zai.glm-5.3
model_fallbacks:
- gemini:gemini-3.8-flash
- codex:gpt-6.1-sol
- openai:gpt-6.1-sol
- claude:claude-sonnet-5-5
compaction_agent: compact-planner
use_tools:
- bash_exec
- bash_read_exec_log
- bash_spawn
- bash_wait
- bash_status
- bash_terminate
- fs_read
- fs_ls
- fs_grep
- fs_find
- librarian_session_prompt
- librarian_session_send
- librarian_session_wait
- librarian_session_load

- pytheas_session_prompt
- pytheas_session_send
- pytheas_session_wait
- pytheas_session_load

- fs_rollback_file
- harnx_agent_session_history_read
- attachments_attachment_read
description: "Pre-planning consultant \u2014 analyzes user requests before Daedalus\
  \ generates plans, identifying hidden intentions, ambiguities, and AI failure points.\
  \ Produces directives that guide the planner. Named after Metis (MEE-tis), the Greek\
  \ goddess of wisdom, prudence, and deep counsel.\n"
version: '0.4.19'
variables:
- name: shell_execution
  description: Shared shell execution guidance for commands, tests, and diagnostics
  path: shared/shell-execution.md
- name: async_delegation
  description: Preferred async delegation protocol (session_send/session_wait)
  path: shared/async-delegation.md
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: learnings_search
  description: Protocol for retrieving current repository knowledge and verified history
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

{{async_delegation}}

{{repo_docs}}

{{ast_grep_search}}

{{learnings_search}}

{{shell_execution}}

{{output_style}}
