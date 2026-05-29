---
model: mock:mock-llm
description: Demo agent for tool confirmation recording.
use_tools:
  - time_get_current_time
hooks:
  entries:
    - event: PreToolUse
      type: claude-command
      command: "bash demos/config/ask-confirm-hook.sh"
---

You are a helpful assistant. When asked about the time, use the time tool to check it.
