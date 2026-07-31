# harnx-fs-tools

`harnx-fs-tools` is a high-performance filesystem toolset server that provides secure file and directory operations to LLM agents. It implements safety guards and path access bounds for restricted filesystem access.

## Overview

The server runs as a toolset server (implementing the `Toolset` trait) and also supports `--mcp-stdio` mode for MCP backward compatibility. It restricts all filesystem operations to a set of allowed "roots." If no roots are specified via CLI flags (`--root` or `--default-root-cwd`), all operations are denied by default.

## Installation

To install `harnx-fs-tools` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-fs-tools
```

## CLI Options

| Option | Short | Description |
| :--- | :--- | :--- |
| `--root <PATH>` | `-r` | Add an allowed root directory. This flag can be repeated to allow multiple roots. |
| `--default-root-cwd` | | Seed an allowed root from process CWD when no explicit roots are specified. Protects against home exposure: if CWD is `$HOME` or an ancestor of `$HOME` (or if `$HOME` is unset/unresolvable), seeding is skipped and access is denied with a stderr warning. Explicit `--root` options take precedence. |
| `--mcp-stdio` | | Run in stdio MCP backward-compatibility mode instead of the default toolset mode. |
| `--help` | `-h` | Show the help message. |

## Tools

The server exposes the following filesystem tools:

- `read`: Read file contents with support for pagination, grep filtering, and truncation.
- `write`: Create or overwrite a file with specified content.
- `edit`: Perform exact-text replacement within a file.
- `insert`: Insert text at a specific line and column.
- `re_replace`: Replace text using regular expressions.
- `ls`: List directory contents, optionally recursively.
- `grep`: Search file contents with regex and optional context lines.
- `find`: Find files by glob pattern.
- `rollback_file`: Restore a repository to a prior harnx history snapshot.
