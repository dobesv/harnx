# harnx-bash-tools

`harnx-bash-tools` runs shell commands through a filesystem sandbox. Native toolset mode is the default; `--mcp-stdio` keeps MCP stdio compatibility.

Filesystem access is deny-all unless explicit allow paths or batches are enabled. Write and execute grants also grant read access. `$HOME` and its ancestors are never writable or executable through allow inputs.

## Installation

```sh
cargo install --path crates/harnx-bash-tools
```

## Filesystem allow options

| Option | Description |
| :--- | :--- |
| `--allow-read <PATH>` | Grant read access. Repeatable. |
| `--allow-write <PATH>` | Grant read and write access. Repeatable. |
| `--allow-exec <PATH>` | Grant read and execute access. Repeatable. |
| `--allow-rwx <PATH>` | Grant read, write, and execute access. Repeatable. |
| `--allow-common-default` | Grant common operating-system paths and temporary directories. |
| `--allow-dev-tools` | Grant supported development toolchains and caches. |
| `--allow-repo-work` | Grant detected project paths and session working directory. |
| `--allow-all` | Request full filesystem access, subject to `$HOME` ancestor guard. |

Other options include `--no-sandbox`, `--sandbox-run <PATH>`, `--env`/`-e`, `--mcp-stdio`, and `--help`/`-h`.

## Environment variables

Path-list variables use platform path-list syntax:

- `HARNX_TOOLS_ALLOW_READ`
- `HARNX_TOOLS_ALLOW_WRITE`
- `HARNX_TOOLS_ALLOW_EXEC`
- `HARNX_TOOLS_ALLOW_RWX`

Batch toggles accept `1`, `true`, `yes`, or `on`:

- `HARNX_TOOLS_ALLOW_COMMON_DEFAULT`
- `HARNX_TOOLS_ALLOW_DEV_TOOLS`
- `HARNX_TOOLS_ALLOW_REPO_WORK`
- `HARNX_TOOLS_ALLOW_ALL`

`HARNX_BASH_ENV_PASSTHROUGH` remains a comma-separated list of extra child environment variable names.
