---
harnx: patch
---

fix(packages): bash.yaml proxy-auth hook silently dropped all args after the first

The `harnx-proxy-auth` hook command in `packages/{pantheon,coding}/mcp_servers/bash.yaml`
is a folded (`>-`) YAML scalar. Its jq `then`/`end` lines were indented deeper
than the `--hook` they belonged to, so YAML preserved those as **literal
newlines between arguments**. Because the hook runs via `sh -c`, each newline
was a command separator — `sh` executed `harnx-proxy-auth --hook '<first hook>'`
and discarded everything after it (`--hook` for api.github.com, `--env`, and
`--hook …/jira-auth-hook.py`), reporting `sh: --hook: not found`.

Result: GitHub API (`api.github.com` Bearer) auth was never injected, the acli
config dir was never set, and the Jira auth hook never ran (hence no log file).
Fixed by aligning the jq continuation lines with `--hook` so the scalar folds
to a single space-separated command; verified all arguments now reach
`harnx-proxy-auth`.
