---
harnx: minor
---

feat(nats): migrate `fs` and `bash` MCP servers to run bridged over NATS via `harnx-mcp-bridge`. Add `--default-root-cwd` to `harnx-mcp-fs` and `harnx-bash-tools` to seed allowed roots from process CWD with `$HOME`-ancestor protection. Export ambient `HARNX_PACKAGE_DIR` to NATS tool servers so bundled hooks resolve when wrapped in `harnx-mcp-hooks-proxy`.

**Behavior change**: Running fs/bash from `$HOME` (or a `$HOME`-ancestor directory) now denies access with a warning — the `$HOME`-ancestor guard blocks the CWD default. To allow operations from `$HOME`, pass `--root $HOME` explicitly. References #1224.
