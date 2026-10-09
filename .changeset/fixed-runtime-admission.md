---
harnx: patch
---

Add fixed runtime admission tickets and non-executable closure for coordinated frontends. Delayed prompt appends cannot move past a closed admission, and retries resolve the original identity without duplicate prompts. Deploy readers that understand the new `admission_closed` transcript entry before enabling writers.
