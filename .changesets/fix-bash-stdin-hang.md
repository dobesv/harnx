---
harnx: patch
---
Fix MCP bash tool hanging by setting `stdin(Stdio::null())` on spawned commands. Without this, child processes inherited the MCP transport's stdin pipe, causing commands to block indefinitely waiting for input that would never arrive.
