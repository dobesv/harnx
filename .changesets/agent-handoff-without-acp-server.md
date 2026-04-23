---
harnx: patch
---
Agent handoff (`*_session_handoff` tools) now works without an explicit ACP server entry — the handoff tool declarations are generated from the set of known agents and are no longer gated on `acp_manager.is_some()`. Fixes the real-world failure reported in #303 where daedalus→atlas handoff printed the final message and stopped instead of switching agents.
