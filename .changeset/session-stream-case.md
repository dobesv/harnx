---
harnx: patch
---
Preserve case in session stream names so distinct short session IDs do not share a transcript or fail to append. Start fresh sessions after upgrading; previous uppercase stream names are not migrated. Remove unavailable or incorrectly named tool suggestions from sub-agent errors and runtime notes while retaining the child session ID.
