---
harnx: minor
---

feat(hooks): inject `$HARNX_PACKAGE_DIR` into hook processes

Every hook command now runs with a `HARNX_PACKAGE_DIR` environment variable set
to the directory of the package that owns the hook (for hooks defined by a
packaged MCP server), falling back to the config directory for hooks defined
outside a package. This lets packages bundle helper scripts alongside their
config and reference them without hardcoding an absolute path, e.g.
`harnx-proxy-auth --hook $HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py`.
