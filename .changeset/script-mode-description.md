---
harnx: patch
---
Replace the script-mode `bash` placeholder description with a generated guide covering available commands, schema-derived tool synopses, quoting, Monty Python, exit codes and execution limits. Include a tool-specific pipeline when a compatible file reader is available, and show binary attachment creation only when the tool schema supports base64 encoding.

Probe attachment metadata (`stat`, `test -f`) without downloading file contents or consuming cache budget. Allow distinct attachments to download concurrently, with atomic cache-budget reservations that prevent in-flight downloads from exceeding the byte limit. Fail fast with a clear error when the attachment cache is full.
