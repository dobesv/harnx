---
role: subagent
model: gemini:gemini-3-flash-preview
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
- harnx_agent_session_history_read
description: "Testing adequacy specialist — evaluates test coverage, edge case handling, assertion quality, test isolation, and identifies untested code paths. Named after Thalia (thuh-LY-uh), the Muse of comedy — finding what's absurdly untested.\n"
version: '0.3.2'
variables:
- name: ast_grep_search
  description: Guide for structural code search with ast-grep
  path: shared/ast-grep-search.md
- name: repo_docs
  description: Instructions for discovering repository documentation
  path: shared/repo-documentation-discovery.md
- name: thalia_core
  description: Core identity and instructions for Thalia
  path: shared/thalia.md
- name: output_style
  description: Output style rules for concise, low-verbosity responses
  path: shared/output-style.md
- name: policy_test_coverage
  description: Test coverage policy — trigger, exemptions, opt-out rules
  path: shared/policy-test-coverage.md
- name: muse_output_format
  description: Output format and verification requirements for Muse findings
  path: shared/muse-output-format.md
---

{{thalia_core}}

{{policy_test_coverage}}

{{repo_docs}}

{{ast_grep_search}}

{{output_style}}

{{muse_output_format}}
