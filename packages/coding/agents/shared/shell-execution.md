## Shell Execution

When running commands, tests, or diagnostics:
- Prefer `bash_spawn` over `bash_exec` for builds, tests, linters, and long-running operations. Set `timeout_secs` a little above the command's usual run time, so reaching it means the command is stuck rather than slow, and add `wait: {}` (up to 240 seconds) or `wait: {forever: true}` to wait for the result.
- If the wait expires, the command keeps running in the background. Wait expiry is not process termination.
- Check a spawned command without waiting using `bash_status`. Continue waiting with `bash_wait` (`seconds` 1 to 240, or `forever: true`), or stop the process with `bash_terminate`. A spawn killed at its `timeout_secs` deadline reports `status: timed_out`.
- If `stopped_by: new_message` is returned, read the message. `status: running` means command still runs (spawn/`bash_wait`); `status: interrupted` means foreground process stopped, as with templates set to terminate on new messages. Re-run if still needed. `stopped_by: caller_deadline` means your own run deadline is near; report now instead of waiting again.
- `bash_exec` remains available for quick synchronous commands where background lifecycle management is unnecessary. `bash_exec` and spawn deadlines (`timeout_secs`) run from 1 second to 24 hours, which is also the default. Give `bash_exec` a `timeout_secs` well past the command's usual run time, so nothing gets killed and rerun.
