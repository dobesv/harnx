## Shell Execution

When running commands, tests, or diagnostics:
- Prefer `bash_spawn` with nested `wait` (e.g. `wait: { timeout_secs: 30 }`) over `bash_exec` for builds, tests, linters, and long-running operations.
- If wait expires, the command continues running in the background — wait expiry is not process termination.
- Call `bash_wait` to continue waiting, or `bash_terminate` to stop the process.
- Bounded waits under 5 minutes are recommended to stay within common LLM prompt cache TTLs (120 seconds is default).
- `bash_exec` remains available for quick synchronous commands where background lifecycle management is unnecessary.
