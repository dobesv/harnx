---
harnx: minor
---

Add a startup message to the harnx-proxy-auth exec-hook protocol. After a hook
prints `READY`, the proxy sends `{"event": "startup", "vars": {...}}` and the
hook may respond with an `env` map that is injected into the sandboxed command
(and write files to `temp_file_root`) before the first request runs. The bundled
`jira-auth-hook.py` now initializes eagerly at startup.
