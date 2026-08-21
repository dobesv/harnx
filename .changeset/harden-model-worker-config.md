---
harnx: patch
coding: patch
---
Prevent test environment races from overwriting user configuration, preserve embedded model metadata alongside custom client models, and validate shipped agent model references. Model-list APIs now evaluate the current client configuration on every call and return owned values. Add the missing Gemini client used by the coding package's compaction agent.
