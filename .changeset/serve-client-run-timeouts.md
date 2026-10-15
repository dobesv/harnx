---
harnx: minor
---
Add `timeout_secs` to AG-UI and JSON-RPC session/prompt APIs, allowing HTTP clients
to shorten (but not extend) configured run limits. Deadline terminations emit explicit
RUN_ERROR on local/remote follow and replay, distinguishable from manual cancellation.
