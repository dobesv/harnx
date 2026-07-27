---
harnx: patch
---

Fix the TUI incorrectly switching to idle state while a prompt task is still active. Sub-agent events are now represented using a structural `AgentEvent::SubAgent` variant, replacing out-of-band source tracking. When a nested sub-agent completes or fails, its output is rendered under its own source heading without clearing the main task's busy spinner.
