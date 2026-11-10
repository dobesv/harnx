---
harnx: patch
---
The tool invocation journal bucket (`harnx_tool_invocations`) is now S2-compressed. An existing bucket is switched over when a tool server or worker next opens the journal; each NATS server applies the change to its copy after it restarts, and only to blocks it writes or rewrites from then on, so rows already on disk stay uncompressed until their block is rewritten.
