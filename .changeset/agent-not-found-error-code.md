---
harnx: patch
---

Add a stable machine-readable `error.code` of `agent_not_found` to the HTTP 404 body `harnx-serve` returns when an agent does not exist or access rules hide it from the caller. The status, message, and `invalid_request_error` type are unchanged, hidden and unknown agents still answer byte-identically so callers cannot enumerate agents, and no other error (including other 404s such as `session not found`) carries a `code`, so clients no longer have to match the English message text to tell an agent-level refusal from other failures. Internally the status and `code` travel in a typed error frame separate from the message text, so caller-controlled text — agent refs, cursor payloads, query keys — can never collide with an annotation or spoof a status or code.
