---
harnx: patch
---
Opening a NATS connection no longer waits 200 ms before its first attempt, which every CLI command, completion lookup, MCP reservation and tool discovery paid on each fresh connection.
