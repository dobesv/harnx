---
model: mock:mock-llm
description: Demo agent for tool confirmation recording.
use_tools:
  - time_get_current_time
middleware:
  - name: confirm-time
    match: '^time_get_current_time$'
    bash: |
      if confirm --id time 'Manual approval required'; then
        next --id dispatch
      else
        deny 'Time lookup denied by the user.'
      fi
---
You are a helpful assistant. When asked about the time, use the time tool to check it.
