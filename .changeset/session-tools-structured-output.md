---
harnx: patch
---

Session history and metadata tools now return JSON values in Bash scripts and Python run inside `bash`. In Bash, `-o json` now prints the payload instead of the MCP envelope. If a script reads `.content[0].text` in that mode, update it to use the payload directly. `help` lists the metadata fields. Direct calls still return JSON text. Text pipes also omit user-only inline media after attachment conversion.
