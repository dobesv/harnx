---
harnx: patch
---
Fix AG-UI session creation and following of unclaimed prompts.

JSON-RPC `session/prompt` now creates an absent session, matching the AG-UI run endpoint. Clients that choose their own session IDs can send their first prompt without creating the session separately.

Report admitted, unanswered prompts as running on other replicas before a worker claims them. Promptless AG-UI attachments now wait for the worker or durable completion, with a bounded lease-acquisition wait that reports an error instead of finishing an unclaimed run.

Keep retained prompts pending when their Error or Cancel entries are deleted, and hydrate completed replies before finishing remote attachments that observed a worker lease.
