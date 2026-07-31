# harnx-mcp-bash

`harnx-mcp-bash` is an MCP server providing controlled shell command execution (`exec` and `spawn`) bounded by sandbox rules and filesystem roots.

## Overview

The server communicates via stdio or bridged over NATS. Command execution is restricted to allowed filesystem roots and extra path allowlists. If no roots or allowlists are configured, command execution and working directory resolution are denied by default.

## Installation

To install `harnx-mcp-bash` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-mcp-bash
```

## CLI Options

| Option | Short | Description |
| :--- | :--- | :--- |
| `--root <PATH>` | `-r` | Add an allowed root directory. Repeatable. |
| `--default-root-cwd` | | Seed an allowed root from process CWD when no explicit roots are specified. Protects against home exposure: if CWD is `$HOME` or an ancestor of `$HOME` (or if `$HOME` is unset/unresolvable), seeding is skipped and access is denied with a stderr warning. Explicit `--root` or client roots take precedence. |
| `--extra-read <PATH>` | | Grant read-only access to an additional path outside allowed roots. |
| `--extra-write <PATH>` | | Grant write access to an additional path outside allowed roots. |
| `--extra-exec <PATH>` | | Grant execute access to an additional path outside allowed roots. |
| `--extra-rwx <PATH>` | | Grant read, write, and execute access to an additional path. |
| `--help` | `-h` | Show the help message. |

## Environment Variables

Additional path access can also be granted via environment variables:

- `HARNX_BASH_EXTRA_READABLE`: Colon-separated list of extra readable paths.
- `HARNX_BASH_EXTRA_WRITABLE`: Colon-separated list of extra writable paths.
- `HARNX_BASH_EXTRA_EXEC`: Colon-separated list of extra executable paths.
- `HARNX_BASH_EXTRA_RWX`: Colon-separated list of extra read/write/executable paths.
