---
harnx: minor
---

Add base64 encoding support to `attachment_create` for binary attachments, with encoded-length and decoded-size checks. Require UTF-8 decoded bytes for text MIME types so created attachments remain readable. Document the broker request limit separately from the 5 MiB handler ceiling. Show a schema-gated PNG creation idiom in the script-mode `bash` description using the agent-visible tool name, including server and package prefixes.
