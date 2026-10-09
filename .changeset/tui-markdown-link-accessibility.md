---
harnx: patch
---

Show full, keyboard-selectable URLs from Markdown tool bodies, tool results,
and subagent prompts and replies. Expose links in streamed assistant text before
and between tool calls, without waiting for the final reply. Extract result links
before preview truncation, including links added by custom result templates. Replace tool-body links on live
updates and keep subagent reply links ordered and deduplicated on repeated events.
Preserve tool-result detail pairing and match live link extraction when restoring
session history.
