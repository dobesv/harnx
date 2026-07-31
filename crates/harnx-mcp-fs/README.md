# harnx-mcp-fs

`harnx-mcp-fs` is a high-performance MCP filesystem server that provides secure file and directory operations to LLM agents. It implements safety guards and supports dynamic MCP roots for restricted filesystem access.

## Overview

The server communicates via stdio using the Model Context Protocol (MCP) or bridged over NATS. It restricts all filesystem operations to a set of allowed "roots." If no roots are specified via CLI or provided dynamically by the MCP client, all operations are denied by default.

## Installation

To install `harnx-mcp-fs` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-mcp-fs
```

## CLI Options

| Option | Short | Description |
| :--- | :--- | :--- |
| `--root <PATH>` | `-r` | Add an allowed root directory. This flag can be repeated to allow multiple roots. |
| `--default-root-cwd` | | Seed an allowed root from process CWD when no explicit roots are specified. Protects against home exposure: if CWD is `$HOME` or an ancestor of `$HOME` (or if `$HOME` is unset/unresolvable), seeding is skipped and access is denied with a stderr warning. Explicit `--root` or client roots take precedence. |
| `--help` | `-h` | Show the help message. |

## Tools

The server exposes the following tools to the MCP client:

- `read`: Read file contents with support for pagination, grep filtering, and truncation.
- `write`: Create or overwrite a file with specified content.
- `edit`: Perform exact-text replacement within a file.
- `insert`: Insert text at a specific line and column.
- `re_replace`: Replace text using regular expressions.
- `ls`: List directory contents, optionally recursively.
- `grep`: Search file contents with regex and optional context lines.
- `find`: Find files by glob pattern.
- `rollback_file`: Restore a repository to a prior harnx history snapshot.
