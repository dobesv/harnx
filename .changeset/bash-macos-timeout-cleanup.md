---
harnx: patch
---
Fix macOS bash command timeouts failing with "Operation not permitted" when a process group has already exited. Continue cleanup after failed SIGTERM and tolerate kill errors for exited processes while preserving process identity checks.
