---
harnx: patch
---
Derive session stream names from the exact session ID using SHA-256 so distinct IDs do not share a transcript or fail to append, including on case-insensitive filesystems. Start fresh sessions after upgrading; earlier stream names are not migrated. Remove unavailable or incorrectly named tool suggestions from sub-agent errors and runtime notes while retaining the child session ID.
