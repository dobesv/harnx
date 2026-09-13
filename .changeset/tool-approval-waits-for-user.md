---
harnx: patch
---
Keep TUI tool approvals pending until the user responds instead of automatically denying them after 30 minutes. Cancellation and frontend shutdown still end the approval wait.

Preserve synchronous confirmation support on both current-thread and multithreaded Tokio runtimes.
