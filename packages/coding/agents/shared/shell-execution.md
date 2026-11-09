## Shell Execution

When running commands, tests, or diagnostics:
- Prefer `bash_spawn` with nested `wait` (e.g. `wait: { timeout_secs: 30 }`) over `bash_exec` for builds, tests, linters, and long-running operations.
- If wait expires, the command continues running in the background — wait expiry is not process termination.
- Call `bash_wait` to continue waiting, or `bash_terminate` to stop the process. A spawned command has no deadline of its own: terminate it when it runs well past its usual time with no new output.
- Bounded waits under 5 minutes are recommended to stay within common LLM prompt cache TTLs (120 seconds is default).
- `bash_exec` remains available for quick synchronous commands where background lifecycle management is unnecessary. Give it a `timeout_secs` well past the command's usual run time, so reaching it means the command is stuck rather than slow and nothing gets killed and rerun; without one, the limit is 24 hours.
