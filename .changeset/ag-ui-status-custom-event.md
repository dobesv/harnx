---
harnx: patch
---

harnx-serve now maps `AgentEvent::Status` to an AG-UI `CUSTOM_EVENT` (name `status`, payload `{ "text": string }`), emitted within run boundaries, so web clients can surface agent status the way the TUI does. Previously these status updates were dropped.
