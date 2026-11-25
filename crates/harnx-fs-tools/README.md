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

- `read`: Read file contents with pagination, grep filtering, and truncation. Lines are prefixed with their number (`1: text`); pass `line_numbers: false` to omit them.
- `write`: Create or overwrite a file. To copy attachment bytes (including binary), use `write_attachment`.
- `write_attachment`: Copy exact bytes from a `cid:media:` attachment, or rendered Markdown from a `cid:plan:` URL, to a file. Creates or overwrites destination without text decoding.
- `edit`: Perform exact-text replacement.
- `insert`: Insert text at a line and column.
- `re_replace`: Replace text using regular expressions.
- `ls`: List directory contents.
- `grep`: Search file contents with a regex.
- `find`: Find files by glob pattern.
- `rollback_file`: Restore a repository to a prior harnx history snapshot.

### Output in `bash` scripts

This applies when the `bash` tool calls `harnx-fs-tools` natively, behind `harnx-mcp-bridge` with `--mcp-stdio`, or through the k8s sandbox gateway. A server built before the request travelled over MCP answers in the direct-call style, and the command then prints that text.

When a `bash` script calls these tools, they behave like the shell commands of the same name. Data goes to stdout, notes go to stderr as `fs_<tool>: <note>`, and the exit status says whether anything was found.

- `fs_read` prints the selected lines as they are in the file, with no line-number prefix unless you pass `--line-numbers`, and adds no final newline to a file that has none, so `fs_read --path f > copy` matches `f` byte for byte when `f` is valid UTF-8 text and no limit cuts the output. Invalid UTF-8 bytes are printed as U+FFFD, and a binary file is refused. A line cap or byte cap truncates the output and prints a note on stderr naming the `offset` to continue from, and a single line longer than the byte cap is cut with a note.
- `fs_ls` prints one entry name per line. `fs_find` prints one path per line, and `fs_grep` prints `path:line:text` rows; their paths start with the `--path` you gave, so they can be passed straight to `fs_read --path`. `fs_grep --path` accepts a file as well as a directory; for a file the printed path is the argument itself.
- `fs_grep` exits 1 when nothing matched. Scripts run with `set -e`, so a bare no-match `fs_grep` ends the script: use it in `if` or `||`, or add `|| true`. It exits 2, with the reason on stderr, when the path it was given can't be read, is not a regular file or directory, is over the search size limit or is a binary file; unreadable, oversized or binary files found inside a searched directory are skipped without a report.
- `fs_find` and `fs_grep` stop at `max_results`, `fs_ls` at 500 entries, and all three at 128 KiB of output, each with a note on stderr.

```bash
fs_find --pattern '*.rs' --path src | while read -r p; do fs_read --path "$p" | grep -n TODO | head -3; done
if fs_grep --pattern TODO --path src >/dev/null; then echo found; fi
```

`-o json` prints the result's fields instead: `fs_read` gives `text`, `notes`, `path`, `total_lines`, `start_line` and `end_line`; `fs_ls` gives `entries` and `notes`; `fs_find` gives `paths` and `notes`; `fs_grep` gives `matches` (rows of `path`, `line`, `text` and, for context lines, `context`), `notes` and `exit_code`. A direct call from a model is unchanged except that `fs_read` takes `line_numbers` (default true there, false in a script) and `fs_grep` searches a file when `path` names one.

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
