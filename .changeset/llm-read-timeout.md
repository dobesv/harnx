---
harnx: patch
---
Add a configurable read (inactivity) timeout for LLM provider HTTP requests so stalled responses fail with a clear error instead of hanging until the ACP idle timeout; raise the default ACP idle timeout to 600s as a backstop.
