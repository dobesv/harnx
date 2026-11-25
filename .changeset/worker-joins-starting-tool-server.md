---
harnx: patch
---
A worker session that needs a tool server another session is still starting now waits for that start, instead of discovering tools before the server has registered and finding none of its tools.
