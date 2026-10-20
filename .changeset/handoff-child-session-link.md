---
harnx: minor
---

Reserve target child session ID during handoff tool execution and include it in the handoff tool result. The result now includes the target agent and child session ID in text (`Handing off to <agent>; target session: <id>.`) and in a structured `handoff: {agent, session_id}` marker without a user-only audience restriction. Parent model context and session dumps now include the target session ID, and web and TUI interfaces render a static navigable row linking to the child session. Issue #1834.
