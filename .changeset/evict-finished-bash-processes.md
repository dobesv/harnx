---
harnx: patch
---
Bound retained background bash process results and cached diffs with a 30-minute exit TTL and a 128-result cap. Keep in-flight calls working after eviction and report recently evicted execution IDs as finished and no longer tracked.
