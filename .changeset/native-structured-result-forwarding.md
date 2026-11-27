---
harnx: patch
---

`harnx-mcp-remote`'s native mode now forwards the structured-result request to the upstream server. A `tools/call` from a `bash` script's inner call (or any caller that asked for `harnx:structured_result`) carries `"harnx:structured_result": true` in the request `_meta`, so an upstream that supports it answers with script-style `structuredContent` — mapped stdout, stderr and exit status in BashKit, `-o json` output, and a `dict` from Python's `tools.NAME(...)`. Calls that did not ask keep model-style results: the capability is sent only when the caller requested it, and trace-context params still ride along beside it. This matches what `harnx-mcp-bridge` and the k8s sandbox already did.
