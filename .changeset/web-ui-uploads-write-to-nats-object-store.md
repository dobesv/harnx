---
harnx: patch
---

Web UI attachment uploads are now written to the shared NATS `harnx_attachments` object store before the upload response returns, so a prompt that references the uploaded file is admitted by any replica (or the same pod after a restart) instead of failing with HTTP 409 because the blob only existed in the receiving pod's local attachments directory. The local file is kept as a warm cache. If the object-store write fails, the upload now returns an explicit 5xx (503 when the broker is unreachable, 500 for other store failures) with no attachment refs, instead of silently returning a ref that other replicas cannot resolve; malformed, oversized, or unsupported-type parts keep their existing 4xx responses.
