---
harnx: patch
---

Remove the `inputs`/`outputs` parameters from the bash MCP tools (`bash_exec`/`bash_spawn`). The sandbox no longer narrows project roots per call — roots always get read+write+exec — fixing `cargo` build failures in sub-agents (#850). Legacy calls that still pass `inputs`/`outputs` are accepted and ignored.
