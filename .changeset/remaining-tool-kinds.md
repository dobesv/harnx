---
harnx: patch
---

Complete static ToolKind declarations for remaining native toolsets (#2096)

Extends the static tool kind declarations to the remaining native toolsets:

- exa: web_search_exa -> Search, web_fetch_exa -> Fetch
- k8s-sandbox: connect -> Execute, status -> Read, release -> Delete
- subagent: session_new -> Other, session_prompt -> Execute, session_load -> Read, session_cancel -> Delete

Each toolset now has unit tests asserting `tool.kind()` for all declared specs.
