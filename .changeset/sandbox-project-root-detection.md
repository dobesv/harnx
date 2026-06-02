---
harnx: minor
---

This release introduces project-root pseudo-variables for sandbox path configuration in `harnx-sandbox-run` and `harnx-mcp-bash`. You can now use `$GIT_ROOT`, `$GIT_COMMON_DIR`, `$NODE_PROJECT_ROOT`, `$CARGO_ROOT`, and `$GO_ROOT` in both CLI flags (`--extra-*`) and environment variables (`HARNX_BASH_EXTRA_*`).

These variables are resolved at startup against the current working directory. If you are not inside a matching project (e.g., you use `$GIT_ROOT` while not in a git repository), the path is silently skipped. For security, any path that resolves to your home directory or an ancestor of it is also dropped.

Additionally, `harnx-mcp-bash` now correctly applies the home-directory guard to all extra paths provided via flags or environment variables, matching the security behavior of `harnx-sandbox-run`.
