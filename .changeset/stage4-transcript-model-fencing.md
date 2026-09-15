---
harnx: patch
---
Fence worker transcript, model completions, hook replies and work admission by execution generation. Interrupted generations no longer publish normal assistant/tool completion output. Requires coordinated worker and hook-server deployment; cancellation still waits for physical cleanup where it did before.
