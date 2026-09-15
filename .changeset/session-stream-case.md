---
harnx: patch
---
Scope sessions by the exact agent name and local session ID, allowing different agents to reuse IDs such as `review-12345` independently. Use SHA-256 storage and stream names to preserve case sensitivity on all filesystems. Require an explicit agent for session commands. Start fresh sessions after upgrading; earlier stream names are not migrated. Remove unavailable or incorrectly named tool suggestions from sub-agent errors and runtime notes while retaining the child session ID.
