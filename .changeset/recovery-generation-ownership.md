---
harnx: patch
---
Recover original execution ownership before resuming a session. Stopped generations no longer replay legacy tools or pass their pending prompts to a new generation. Restart repairs missing cancellation records from committed stop decisions; normal, uninterrupted recovery still resumes.
