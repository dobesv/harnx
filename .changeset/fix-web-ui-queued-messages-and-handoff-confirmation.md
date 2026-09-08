---
harnx: patch
---

Fix Web UI parity for queued messages (#1741) and tool-approval confirmations (#1742). Queued messages in the web UI can now be viewed, edited (restored to composer), or cancelled. Tool approval and session handoff confirmations now display in the web UI, accept approve/deny decisions, and reappear for clients that reconnect while an approval is pending. Mid-turn injection timing for queued messages and NATS tool-approval fanout redesign remain deferred.
