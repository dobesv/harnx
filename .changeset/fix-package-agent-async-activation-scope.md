---
harnx: patch
---
Fix package agents losing their delegation tools when activated directly (#826).

When a package agent (e.g. `pantheon/atlas`) was activated through the async
`Config::use_agent` path — used by `--agent`, handoff `switch_agent`, and the
ACP server — the MCP/ACP managers were never re-scoped to the agent's package.
They stayed in the global scope left by `Config::init`, so every package server
was emitted with a `<package>__` prefix. Two visible symptoms resulted:

- The agent's own same-package ACP delegation tools were emitted as
  `<package>__<peer>_session_prompt` instead of the bare `<peer>_session_prompt`
  its `use_tools` allow-list references, so they were filtered out and the agent
  could not delegate.
- Same-package MCP tools leaked in under both `<package>__*` and sibling-package
  namespaces (e.g. `coding__*`) instead of their bare same-package names.

The intermittency depended on which activation path ran: the synchronous
`use_agent_obj` path already scoped the managers, while the async `use_agent`
path did not. `use_agent` now mirrors `use_agent_obj` and re-scopes the managers
to the incoming agent's package before the agent's tools are snapshotted.
