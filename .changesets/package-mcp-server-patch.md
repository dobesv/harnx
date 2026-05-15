---
harnx: minor
---
Add `mcp_servers` support to package patch files. The `<pkg>.patch.yaml` file now accepts an `mcp_servers` map (regex keys, same as `agents` and `clients`) with fields: `enabled`, `args` (replace), `args_append` (append to existing), `env` (merge), `roots` (replace).
