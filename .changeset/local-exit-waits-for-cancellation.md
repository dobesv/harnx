---
harnx: patch
---
Keep locally owned workers alive until interrupt-and-exit cancellation is confirmed, prevent worker preparation from being misreported as a Ctrl+C request timeout, recover cancellations interrupted during sub-agent startup, and let TUI, CLI, and Web UI users explicitly resume a session by abandoning an unconfirmed execution whose owners disappeared.
