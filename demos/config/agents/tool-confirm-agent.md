---
model: mock:mock-llm
description: Demo agent for tool confirmation recording.
use_tools:
  - time_get_current_time
hooks:
  entries:
    - command: harnx-claude-compatible-hook-server --event PreToolUse --matcher "time_get_current_time" -- bash demos/config/ask-confirm-hook.sh
---
You are a helpful assistant. When asked about the time, use the time tool to check it.
