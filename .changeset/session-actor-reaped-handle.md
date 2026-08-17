---
harnx: patch
---

Fix spurious 503s from `harnx serve` when a request lands on a session whose actor is reaping itself.

An idle session actor stops after 5 seconds and removes itself from the registry. It used to do that without regard for callers, so a request that had already picked up its handle sent commands into a closed channel and got `session actor unavailable` or `session actor dropped ... reply` back as a JSON-RPC 503 — for a session that was perfectly resumable. The reap now happens atomically with the liveness check: an actor only removes itself while the registry holds the last handle to it, so no in-flight request can be talking to it, and otherwise it waits another interval. Handing out a handle also treats an entry whose channel is closed as no actor at all and spawns a replacement, so a session survives an actor task dying on its own (a panic) instead of failing every later request for that key.
