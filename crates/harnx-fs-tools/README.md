# harnx-fs-tools

`harnx-fs-tools` provides bounded filesystem operations to agents. It runs as a native toolset server and supports `--mcp-stdio` for MCP backward compatibility.

Access is denied when no allow inputs are configured. Read and write permissions are separate; write, exec, and rwx grants also permit reads. Filesystem tools don't execute programs, so exec grants act as read grants here.

## Installation

```sh
cargo install --path crates/harnx-fs-tools
```

## CLI options

| Option | Description |
| :--- | :--- |
| `--allow-read <PATH>` | Allow reads under path. Repeatable. |
| `--allow-write <PATH>` | Allow reads and writes under path. Repeatable. |
| `--allow-exec <PATH>` | Allow reads under path. Repeatable. |
| `--allow-rwx <PATH>` | Allow reads and writes under path. Repeatable. |
| `--allow-common-default` | Allow common operating-system paths. |
| `--allow-dev-tools` | Allow development tool and cache paths. |
| `--allow-repo-work` | Allow detected project roots and current directory. |
| `--allow-all` | Allow all filesystem paths. |
| `--mcp-stdio` | Run in stdio MCP backward-compatibility mode. |
| `--help`, `-h` | Show help. |

Path lists can also come from `HARNX_TOOLS_ALLOW_READ`, `HARNX_TOOLS_ALLOW_WRITE`, `HARNX_TOOLS_ALLOW_EXEC`, and `HARNX_TOOLS_ALLOW_RWX` environment variables. Separate paths with a colon on Unix or a semicolon on Windows. Batch toggles use the corresponding `HARNX_TOOLS_ALLOW_COMMON_DEFAULT`, `HARNX_TOOLS_ALLOW_DEV_TOOLS`, `HARNX_TOOLS_ALLOW_REPO_WORK`, and `HARNX_TOOLS_ALLOW_ALL` variables. Values `1`, `true`, `yes`, and `on` enable a batch.

## Tools

- `read`: Read file contents with pagination, grep filtering, and truncation.
- `write`: Create or overwrite a file.
- `edit`: Perform exact-text replacement.
- `insert`: Insert text at a line and column.
- `re_replace`: Replace text using regular expressions.
- `ls`: List directory contents.
- `grep`: Search file contents with a regex.
- `find`: Find files by glob pattern.
- `rollback_file`: Restore a repository to a prior harnx history snapshot.
