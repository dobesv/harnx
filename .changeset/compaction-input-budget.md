---
harnx: patch
---
Strip inline base64 data URLs and structured image/audio payloads from compaction transcripts and fit summarizer input to its model's estimated context budget. Reduce tool output and older messages first, preserve recent requests and replies when they fit, and use marked excerpts or counted omissions for oversized histories. Excerpt larger latest messages first so a large reply doesn't erase a short request. Avoid repeated full-transcript rendering while fitting large sessions.
