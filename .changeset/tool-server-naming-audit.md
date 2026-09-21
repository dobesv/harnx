---
harnx: minor
---

Rename the native time toolset from `harnx-time-server` to `harnx-time-tools` and rename the Git history library from `harnx-mcp-history` to `harnx-git-history`.

Remove the redundant `harnx-mcp-time` server; use `harnx-time-tools --mcp-stdio` for standalone MCP. Remove `harnx-mcp-plans-github` and its internal `harnx-mcp-plans-core` library and `harnx-mcp-plans-hermetic` test binary.

This release includes breaking container packaging changes: the `harnx-mcp-time` GHCR image is no longer published, and `ghcr.io/dobesv/harnx-mcp-plans` is now `ghcr.io/dobesv/harnx-plans-tools`. The plans Dockerfile and CI/release jobs now use the `harnx-plans-tools` name.
