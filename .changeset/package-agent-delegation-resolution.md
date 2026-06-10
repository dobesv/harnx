---
harnx: patch
---
Fix package-relative agent resolution for delegation tools (`_session_prompt`, etc.); tool names now match the slash-free, package-relative scheme used for handoffs. Fixes #709.
