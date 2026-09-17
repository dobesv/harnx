---
harnx: minor
---

Interruption is now one durable `Cancel` entry in the session log. Ctrl+C returns control as soon as that append is acknowledged; workers watch their own session stream, abort model and tool calls, and cancel running tools, hooks and sub-agents through the tool protocol. Interrupted tool calls get placeholder results and a runtime note so the model knows why they ended. The execution-control KV bucket (`harnx_execution_control`) is no longer used and can be deleted; the "unconfirmed cancellation" and "resume anyway" flows and the `--resume-anyway` flag are removed. Tool protocol is v5: deploy workers, tool servers, hook servers and frontends together.
