---
harnx: patch
---
Local runs no longer fail with "local worker did not publish readiness within 15s" when a tool server is slow or misconfigured. The worker announces readiness before it starts tool servers, hooks and sub-agent toolsets, and the front-end now waits with backoff and a progress notice instead of a fixed deadline. Worker, tool-server and hook-server output is captured to `harnx_worker.log` in the state dir so a server that dies during startup explains itself.

Hook servers take their command as trailing arguments after `--` and run it directly instead of through a shell, matching what the hooks guide already documented. A hook that needs pipes, redirection or variable expansion asks for a shell explicitly: `-- sh -c '...'`.
