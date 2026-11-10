---
harnx: minor
---
Add `harnx call bash '<script>'` CLI subcommand and `.call bash <script>` TUI dot-command to run ad-hoc BashKit scripts directly without triggering an LLM inference turn.

Scripts run in an isolated subshell with an in-process virtual filesystem (`/tmp`), no host filesystem or network access, and exposed inner tools mapped to shell commands. For agentless CLI invocations, Harnx uses bounded static AST analysis to infer literal tool command names and claim backing servers; providing explicit `--tools` selectors disables inference and supports additive selection alongside named agent configurations. The TUI dot-command executes against the active session's effective tool selection without flags or inference turns.

Operator-initiated scripts run under scoped operator consent: inner tool calls that trigger `PreToolUse` confirmation hooks (`ask`) auto-approve, while normal agent Bash scripts continue to fail closed with exit code 3. Hook `deny` decisions strictly block inner calls, control tools remain excluded, and child sessions spawned by inner tools retain their normal confirmation policies. Nonzero script status reports execution failure with CLI process exit code 1 while preserving the script exit status in the tool result payload.

Named agent CLI invocations and TUI `.call bash` execute on the tool worker, so workers must be upgraded and restarted together with the frontend; a worker built before this feature rejects the Bash command with a clean error. Agentless CLI invocations run in-process.
