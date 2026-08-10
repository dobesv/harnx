---
harnx: minor
---

A worker launching its own tool servers now starts them per session based on the session's agent, instead of one fixed set at startup. A session whose agent uses different tools than the worker's own config gets the right servers, and a worker with several agents no longer pays for tool servers a given session never calls. A server with no active session using it lingers briefly (to survive back-to-back sessions reusing it) before it actually stops.

One consequence: a tool server that failed to register no longer gets retried by a background loop for the lifetime of the worker process. It now retries the next time some session's agent asks for it — so a server fixed while a long-running session is already active will not come back for that session, only for a new one.
