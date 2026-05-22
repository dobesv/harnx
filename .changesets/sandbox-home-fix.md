---
harnx: patch
---
Fix sandbox `$HOME` exposure via write-path ancestor walk and over-broad roots.

**Issue #619**: `add_write_exception` previously walked the full parent chain to
find the first existing ancestor of a non-existent `--write` path. When
`HOME_RWX_PATHS` entries like `~/.pyenv` or `~/.rye` were absent on disk, the
walk reached `$HOME` and mounted the entire home directory as `WriteAndRead`,
exposing `~/.aws`, `~/.ssh`, and other sensitive directories. Fixed by applying
the same skip-if-missing behavior used by `add_path_exception`: non-existent
write paths are now silently skipped with a warning instead of walking ancestors.

**Issue #503**: CWD and `mcp_root` entries equal to `$HOME` or an ancestor of
`$HOME` (e.g., `/home`, `/`) could be injected as MCP server roots, then granted
`--write`/`--exec` sandbox access. Fixed with defense-in-depth guards at two
points: `reinit_managers_for_agent` (skips inserting the CWD or mcp_root entries)
and `build_sandbox_args` (filters roots from sandbox args). Home subdirectories
(e.g., `$HOME/projects`) remain valid roots and are unaffected.
