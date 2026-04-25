---
harnx: patch
---
Fix Ctrl+C followed by a new submission spawning a second prompt task alongside the first. The TUI now gives each prompt task its own `AbortSignal` (so a fresh submission can never un-abort a running task) and `start_prompt` drains the previous task — cooperatively for ~500ms, then via `JoinHandle::abort` — before spawning a new one. Ctrl+C no longer eagerly clears `llm_busy`; it stays true until the in-flight task actually emits Final/Error, so the user's next message queues into `pending_message` instead of racing the running task. This eliminates the orphan `tool_calls` log entries and missing user messages that appeared in long sessions when a tool round was Ctrl+C'd and resubmitted.
