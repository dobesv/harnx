---
harnx: patch
---
Reserve session metadata before creating a transcript stream for an explicit session ID. Failed metadata writes and rejected worker activations no longer leave empty or error-only session streams behind.
