---
harnx: patch
---
Fix ACP client staying permanently connected after worker crash.

When the ACP subprocess dies or the I/O loop fails after initialization, the
client now detects the dead worker thread on the next tool call and
automatically reconnects instead of returning stale errors indefinitely.

Adds a death-notification channel (`dead_rx`) to `AcpWorkerHandle` and updates
`ensure_connected()` to check worker liveness before trusting the `connected`
flag.
