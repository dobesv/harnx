---
harnx: patch
---
Fix A2A recovery and failed publication handling so execution deadline expiry settles tasks as canceled, including tasks waiting for tool approval. Foreground task reads now recover ownerless working approval continuations instead of leaving them stranded.
