---
harnx: minor
---

# mcp-bash: add `env` parameter to `bash_exec` and `bash_spawn`

Both tools now accept an optional `env` parameter — a key/value object of additional
environment variables to inject for that specific command invocation.

```json
{
  "command": "npm run test",
  "env": { "NODE_ENV": "test", "CI": "1" }
}
```

The per-call variables are layered on top of the server's built-in environment
(sourced from `.env.bash`, `extra_env_passthrough`, and `env_overrides`), making
them the highest-priority override for a single execution without affecting any
other commands. Works in both sandbox and non-sandbox modes.
 Invalid env keys (empty, containing `=`, or NUL) are rejected up front with a clear error.