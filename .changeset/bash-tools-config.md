---
harnx: minor
---

Add `bash_tools` agent configuration field and session override (`.set bash_tools`) to support script mode.

When `bash_tools` is set and resolves to available tools, Harnx synthesizes a single `bash` tool declaration whose parameters mirror `bash_exec` and whose description lists the inner tools available to scripts. The field accepts YAML lists or comma-separated strings with the same wildcard, brace expansion, and toolset selector syntax as `use_tools`. Excluded control tools (`switch_agent`, `*_session_handoff`, `ask_user`) and `bash` itself cannot be placed in `bash_tools`.

While `bash_tools` resolves to tools, the name `bash` is reserved for script mode: an external tool of the same name is not exposed, and a `bash` call fails closed with a clear error rather than routing to it. Script execution itself is under development; in this release, calling the synthesized `bash` tool returns that error.

Mixed-version deployments: session metadata carrying a `bash_tools` override uses strict field checking (`deny_unknown_fields`), so worker binaries older than this release cannot load it. Upgrade all workers before setting `bash_tools` overrides.
