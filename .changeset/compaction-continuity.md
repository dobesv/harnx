---
harnx: patch
---

Fix conversation continuity after compaction (#2388). Model requests now include the stored summary before the retained conversation, without resurrecting archived turns or duplicating user messages after reload. Repeated compaction carries the prior summary forward. Workers skip re-logged copies of completed prompts by stable message ID and preserve new input arriving during compaction.
