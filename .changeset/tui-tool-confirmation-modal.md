---
harnx: patch
---
Fix two TUI rendering issues:

- Tool-use confirmation prompts (`PreToolUse` hooks returning `ask`) now render as a native ratatui modal instead of an `inquire` terminal prompt that collided with the alternate-screen TUI, producing garbled, interleaved output (#695). Answer with `y` to allow; `n`/`Esc`/`Enter` deny.
- The agent welcome banner no longer prints a dangling `v` when an agent has no `version` set — the header now reads `# agent-name` instead of `# agent-name v`.
