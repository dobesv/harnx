---
harnx: minor
---
Add filesystem sandboxing to `harnx-mcp-bash` using the [birdcage](https://crates.io/crates/birdcage) crate. On Linux and macOS, the `exec` and `spawn` MCP tools now run bash inside a sandbox by default — write access is limited to the configured roots and read+execute is limited to system paths needed for bash.

A new helper binary `harnx-mcp-bash-sandbox-run` ships alongside `harnx-mcp-bash`; the server invokes it as a subprocess to set up the sandbox before exec'ing bash. The helper must be in the same directory as `harnx-mcp-bash` (or specified via `--sandbox-run <path>`).

New per-call MCP parameters on `exec` and `spawn`:
- `inputs: string[] | null` — extra read-only paths. `null` (default): no extras beyond system + roots. `[]`: deny-all extra reads, including no working-directory read fallback and suppress the roots-as-read fallback when `outputs` denies writes. `[paths...]`: those paths added as read-only.
- `outputs: string[] | null` — write-permitted paths. `null` (default): roots are writable. `[]`: nothing writable; roots become read-only. `[paths...]`: only those paths are writable, roots are not auto-added.

New `harnx-mcp-bash` server flags (Unix only):
- `--no-sandbox` — disable sandboxing (restore prior unsandboxed behavior).
- `--extra-readable <path>` (repeatable) — additional read-only paths.
- `--extra-exec <path>` (repeatable) — additional exec paths.
- `--sandbox-run <path>` — override the helper-binary location.

New env vars:
- `HARNX_BASH_EXTRA_READABLE` — colon-separated extra read-only paths.
- `HARNX_BASH_EXTRA_EXEC` — colon-separated extra exec paths.

Windows: no sandboxing change (existing Job Object protection preserved). If the helper binary is missing on Unix at startup, the server now fails fast with a clear startup error. Passing `--no-sandbox` is the only way to disable sandboxing and run unsandboxed explicitly.
