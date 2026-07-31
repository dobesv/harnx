---
harnx: minor
---

feat(nats): convert `fs` to a toolset server and rename crate/binary `harnx-mcp-fs` → `harnx-fs-tools`.

The `fs` tool server now implements the `Toolset` trait and runs directly, removing the `harnx-mcp-bridge` wrapper process. `--mcp-stdio` mode is retained for backward compatibility. References #1224.
