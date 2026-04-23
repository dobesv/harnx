---
harnx: patch
---
Persist tool calls and their results as separate session-log entries. The old single `Message(Tool, ToolCalls)` entry duplicated the assistant's text, causing the LLM to see the same prose back-to-back and confabulate that prior rounds were being "replayed from cache." New `tool_calls` and `tool_results` log entries pair up at load time; an orphan `tool_calls` at EOF (e.g. process interrupted mid-round) is repaired with synthesized lost-response errors so the reassembled transcript is still a valid alternating user/assistant sequence.
