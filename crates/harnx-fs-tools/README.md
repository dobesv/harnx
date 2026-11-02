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
| `--name <NAME>` | Override the registered toolset name. |
| `--mcp-http` | Serve MCP over Streamable HTTP at `/mcp`. |
| `--sandbox-transport` | Expose private `write_bytes` for the sandbox gateway; requires `--mcp-http`. Raise this server's HTTP request body limit to fit a 100 MiB base64 file. |
| `--host <ADDR>` | Bind address for HTTP mode (default: `0.0.0.0`). |
| `--port <N>` | Bind port for HTTP mode (default: `3003`). |
| `--mcp-stdio` | Run in stdio MCP backward-compatibility mode. |
| `--help`, `-h` | Show help. |

Path lists can also come from `HARNX_TOOLS_ALLOW_READ`, `HARNX_TOOLS_ALLOW_WRITE`, `HARNX_TOOLS_ALLOW_EXEC`, and `HARNX_TOOLS_ALLOW_RWX` environment variables. Separate paths with a colon on Unix or a semicolon on Windows. Batch toggles use the corresponding `HARNX_TOOLS_ALLOW_COMMON_DEFAULT`, `HARNX_TOOLS_ALLOW_DEV_TOOLS`, `HARNX_TOOLS_ALLOW_REPO_WORK`, and `HARNX_TOOLS_ALLOW_ALL` variables. Values `1`, `true`, `yes`, and `on` enable a batch.

## Tools

- `read`: Read file contents with pagination, grep filtering, and truncation.
- `write`: Create or overwrite a file. To copy attachment bytes (including binary), use `write_attachment`.
- `write_attachment`: Copy exact bytes from a `cid:media:` attachment, or rendered Markdown from a `cid:plan:` URL, to a file. Creates or overwrites destination without text decoding.
- `edit`: Perform exact-text replacement.
- `insert`: Insert text at a line and column.
- `re_replace`: Replace text using regular expressions.
- `ls`: List directory contents.
- `grep`: Search file contents with a regex.
- `find`: Find files by glob pattern.
- `rollback_file`: Restore a repository to a prior harnx history snapshot.

### Attachment resolution

`write_attachment` requires an attachment resolver to fetch `cid:media:` and `cid:plan:` URLs:

- **NATS toolset mode**: When `harnx-fs-tools` runs as a native toolset server over NATS, a resolver is automatically configured from the hosting NATS JetStream connection.
- **MCP stdio and HTTP modes**: MCP modes run without NATS by default and have no resolver configured. Calling `write_attachment` in these modes returns a recoverable error: `"Attachment resolution unavailable: no resolver configured for this filesystem server."`
- **Sandbox pods**: In sandbox pods, the filesystem server runs in MCP HTTP mode without NATS access. The central gateway resolves the attachment over its own NATS connection and transfers the bytes to the pod via the private `write_bytes` tool (see below).

## Sandbox byte transport

Enable `--sandbox-transport` (or `HARNX_FS_SANDBOX_TRANSPORT=1`) only on the
filesystem server inside a sandbox pod, together with `--mcp-http`. Startup
rejects it in every other mode, including native NATS hosting and `--mcp-stdio`,
whether enabled by flag or environment variable. The gateway calls the private
`write_bytes(path, data_base64)` tool after resolving an attachment itself.
Both arguments are required strings.
`data_base64` uses standard padded base64; the decoded file must be at most
100 MiB. Invalid or oversized input returns a recoverable error before
modifying the destination. Existing allowlist and history rules still apply.

This mode raises only this server's MCP HTTP request body limit to 140,858,712
bytes ≈ 134.3 MiB (base64 of 100 MiB plus 1 MiB for the envelope). Other modes keep RMCP's
default limit. `write_bytes` is absent from default tool lists and the public
builtin schemas. Pods don't need NATS credentials or a NATS connection.
