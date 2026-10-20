---
harnx: patch
---
Keep the latest user prompt and recent complete tool rounds after compaction when the final turn exceeds the keep-recent token budget, instead of dropping the entire turn. Preserve the final reply when it fits even if the preceding tool round is too large.
