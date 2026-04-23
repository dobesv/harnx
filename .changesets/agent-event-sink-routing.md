---
harnx: patch
---
Route library-layer progress messages (RAG init/load, MCP server connect failures, fetch crawler progress, session/agent/config save confirmations, dry-run echo) through `AgentEventSink` as `Notice` events instead of direct `println!`/`eprintln!`. Output is visually unchanged in CLI mode but no longer pollutes the ACP/serve protocol channels when those modes trigger library code paths.
