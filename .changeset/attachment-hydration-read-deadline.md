---
harnx: patch
---

Limit each attachment download from the NATS stream leader to 30 seconds for all chunks and retries, instead of starting a fresh retry budget for each chunk.
