---
harnx: patch
coding: patch
---
Prevent test environment races from overwriting user configuration, preserve embedded model metadata alongside custom client models, validate shipped agent model references, and restart or reject stale local workers after configuration or executable changes. Model-list APIs now evaluate the current client configuration on every call and return owned values. Because the local-worker readiness protocol changed, restart long-running frontends when upgrading or rolling back. Add the missing Gemini client used by the coding package's compaction agent.
