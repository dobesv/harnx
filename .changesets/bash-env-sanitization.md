---
harnx: minor
---
**Behaviour change** — `harnx-mcp-bash` no longer passes the full host
environment to child bash processes. The child receives only a curated
allowlist (`HOME`, `PATH`, `LANG`, `LANGUAGE`, `USER`, `SHELL`, `TERM`,
`DISPLAY`, `EDITOR`, `NODE_OPTIONS`, `NODE_EXTRA_CA_CERTS`, `PWD`, `SHLVL`,
`LOGNAME`, `TMPDIR`, `TMP`, `TEMP`, plus all `XDG_*` variables) plus any
explicitly opted-in extras. This applies on every platform — including
Windows, where sandboxing remains unavailable but environment curation is
still performed.

To opt vars back in, use any of:

- CLI flags `-e VAR` (passthrough from host) or `-e VAR=VALUE` (explicit
  override), repeatable.
- Env var `HARNX_BASH_ENV_PASSTHROUGH=A,B,C` for a comma-separated list.
- A `$HARNX_CONFIG_DIR/.env.bash` dotfile with `KEY=VALUE` lines for
  persistent secrets.

Precedence (highest wins): CLI > `HARNX_BASH_ENV_PASSTHROUGH` > `.env.bash`
> default allowlist.

**Upgrade impact**: existing workflows that relied on inheriting host env
vars (e.g. `git push` over SSH needing `SSH_AUTH_SOCK`, `gh` needing
`GITHUB_TOKEN`, custom tools reading project-specific vars) need to declare
those vars explicitly. See `docs/bash-mcp-server.md` for recipes.

Closes #375.
