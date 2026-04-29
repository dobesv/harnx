---
harnx: patch
---
`harnx-mcp-bash`: skip dotfile lines with empty keys (e.g. `=value`) so
they don't reach the sandbox as malformed `--env =value` args; extend
the default env allowlist with Windows-specific names (`SYSTEMROOT`,
`SystemRoot`, `WINDIR`, `USERPROFILE`, `USERNAME`, `APPDATA`,
`LOCALAPPDATA`, `COMSPEC`, `HOMEDRIVE`, `HOMEPATH`) so `bash` on Windows
runs with the env it needs after `env_clear()` curation.
