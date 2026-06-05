---
harnx: patch
---

Fix agent handoff from a package agent resolving to the wrong agent. When a package agent (e.g. `pantheon/daedalus`) handed off a session to a same-package agent via a bare `_session_handoff` tool (e.g. `atlas_session_handoff`), the handoff incorrectly targeted the top-level `atlas` instead of `pantheon/atlas`. Handoff targets are now resolved relative to the active agent's package.

Handoff tool names are also now generated with package-namespaced, schema-valid spelling instead of containing a raw `/` (which is rejected by provider function-name schemas): same-package peers use the bare name (`atlas_session_handoff`), cross-package peers use `pkg__agent_session_handoff`, and top-level agents addressed from within a package use `__agent_session_handoff`. The engine decodes these via an exact lookup table so package and agent names containing underscores remain unambiguous.
