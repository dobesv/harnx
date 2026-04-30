---
harnx: minor
---

Improve filesystem sandboxing configuration for `harnx-mcp-bash`:

- Add `--extra-write <path>` CLI flag and `HARNX_BASH_EXTRA_WRITABLE` environment variable for adding extra writable paths to the sandbox (#376).
- The system temporary directory is now writable by default in the sandbox (Linux: `/tmp`; macOS: `/private/tmp` and `$TMPDIR` if set) without requiring explicit configuration (#377).
- Support `~` tilde expansion in all path-related configuration flags (`--root`, `--extra-read`, `--extra-write`, `--extra-exec`) and their corresponding environment variables (#379).
