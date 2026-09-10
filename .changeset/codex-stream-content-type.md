---
harnx: patch
---
Accept successful Codex subscription streams with a missing Content-Type header instead of discarding completions and falling back to an API-key provider. Preserve HTTP status and retry hints for non-JSON streaming errors, and show underlying causes in retry warnings without dumping invalid-stream response bodies.
