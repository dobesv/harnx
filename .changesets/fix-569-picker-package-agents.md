---
harnx: minor
---
Fix agent picker and tab completion not showing agents from packages.

Agents installed in `packages/<pkg>/agents/` now appear in the TUI agent
picker and `.agent` tab completion (they were only loadable by explicit
`harnx -a pkg/name` before). Variable completion for package agents
(`pkg/name VAR=`) also now resolves the correct file.

The `harnx-pkg` binary is now included in the default `argc install` set
and in the Docker image.
