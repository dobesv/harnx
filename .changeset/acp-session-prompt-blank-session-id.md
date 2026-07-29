---
harnx: patch
---

Fix ACP `session_prompt` failing with a bare "Invalid params" when a sub-agent model passes an empty or made-up `session_id`.

Empty or whitespace-only `session_id` values are now treated as omitted and start a new session instead of being forwarded verbatim. Unknown session IDs (in both `prompt` and `cancel`) now return an actionable error telling the model to use a real ID or omit it to start a new session. The `session_prompt` tool and `session_id` parameter descriptions now spell out how to continue a conversation versus start a new one, and warn against inventing session IDs.
