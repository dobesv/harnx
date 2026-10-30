---
harnx: patch
---

Recovered failed tool calls now use the partial result stored with the journaled reply, even if it changed during replay. Recovered sub-agent progress includes the parent's tool call ID so clients can match it to the original call.
The Web UI no longer shows a second row for a failed sub-agent call when progress arrives after the row was restored from the call's partial result.
