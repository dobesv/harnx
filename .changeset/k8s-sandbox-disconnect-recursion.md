---
"harnx": patch
---

fix(k8s-sandbox-tools): stop idle-watcher disconnect from recursing into itself

`harnx-k8s-sandbox-tools` aborted with a Tokio worker stack overflow about 15 minutes after a replica took the idle-watcher lease (#2177). After suspending idle sandboxes, the leader disconnects their MCP sessions through `SessionDisconnect for Arc<dyn McpCaller>`, whose body called `self.disconnect(..)`. Method resolution picked the same `SessionDisconnect` impl instead of `McpCaller::disconnect`, so the first disconnect recursed until the stack ran out. The adapter now calls the inner caller explicitly.
