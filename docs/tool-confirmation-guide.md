# Tool Confirmation Guide

Tool confirmation allows you to inspect and approve tool calls before they execute. This provides a safety layer for destructive operations, an audit trail for sensitive actions, and a way to learn how the agent interacts with your system.

## 1. Quick Start

The fastest way to enable manual confirmation for all tools is to add a hook entry to your `config.yaml`:

```yaml
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --
        printf '%s\n' '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Manual approval required"}}'
```

With this configuration, Harnx will pause and prompt you for every tool call.

## 2. How It Works

Harnx uses the **hooks system** to implement tool confirmation. When a `PreToolUse` hook returns a specific JSON response, the execution flow pauses:

1.  **LLM requests a tool**: The agent decides to run a tool (e.g., `bash_exec`).
2.  **Hook triggers**: Harnx runs your configured `PreToolUse` hook.
3.  **Hook requests confirmation**: The hook returns `{"permissionDecision": "ask"}`.
4.  **User prompted**: Harnx displays the tool name, arguments, and reason in the terminal.
5.  **Execution or Denial**:
    *   If you approve (**y**), the tool runs normally.
    *   If you deny (**N**), the tool is blocked, and the agent receives a "Denied by user" error.

## 3. Configuration Methods

You can configure confirmation hooks globally in `config.yaml` or per-agent in front-matter.

### Method A: External Script

For more complex logic, move the hook to a script file:

```yaml
hooks:
  entries:
    - command: harnx-claude-compatible-hook-server --event PreToolUse -- /path/to/ask-confirm.sh
```

**ask-confirm.sh** (see `demos/config/ask-confirm-hook.sh` for a working example):
```bash
#!/usr/bin/env bash
# Consume stdin (tool payload) even if not used
cat > /dev/null
# Request confirmation
printf '%s\n' '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Manual approval required"}}'
```

### Method B: Selective Confirmation (Matcher)
Use the `--matcher` flag to only require confirmation for specific tools:

```yaml
hooks:
  entries:
    - command: harnx-claude-compatible-hook-server --event PreToolUse --matcher "bash_exec|bash_spawn" -- /path/to/ask-confirm.sh
```
*The matcher uses a regex against the tool name.*

### Method C: Per-Agent Hooks
Enable confirmation only for specific agents by adding the hook to their Markdown front-matter:

```yaml
---
model: openai:gpt-4o
hooks:
  entries:
    - command: harnx-claude-compatible-hook-server --event PreToolUse -- /path/to/ask-confirm.sh
---

You are a helpful assistant with manual tool oversight.
```

## 4. The Confirmation Prompt

When a hook returns `"permissionDecision": "ask"`, you will see a prompt in your terminal:

```text
Hook requires confirmation for tool 'bash_exec'
Reason: Manual approval required
Input: {
  "command": "rm -rf /tmp/test"
}
Allow this tool call? (y/N)
```

*   **Default Behavior**: The default choice is **No** (deny). You must explicitly type `y` to approve.
*   **Agent Feedback**: If denied, the agent receives: `{"error": "Denied by user", "blocked_by_hook": true}`. The agent can then choose to try a different approach or ask you for clarification.
*   **Non-interactive Mode**: If Harnx is running without a TUI/terminal (e.g., in CI or a pipe), tool calls requiring confirmation are **automatically denied**.

## 5. Advanced: Conditional Confirmation

You can write "smart" hooks that only ask for confirmation when operations appear dangerous.

**smart-confirm.sh**:
```bash
#!/usr/bin/env bash
input=$(cat)
tool_name=$(echo "$input" | jq -r '.tool_name')
command=$(echo "$input" | jq -r '.tool_input.command // empty')

# Only inspect shell tools — let other tools proceed
if [[ "$tool_name" != "bash_exec" && "$tool_name" != "bash_spawn" ]]; then
  echo '{}'
  exit 0
fi

# Ask for commands that modify files
if echo "$command" | grep -qE '(rm|mv|cp|chmod|chown|dd|mkfs)'; then
  printf '%s\n' "{\"hookSpecificOutput\":{\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"Command uses potentially destructive operation\"}}"
else
  echo '{}'
fi
```

### Permission Decision Values
Hooks can return these values in `hookSpecificOutput`:
*   `"allow"`: Tool proceeds without prompting.
*   `"deny"`: Tool is blocked immediately (agent gets error).
*   `"ask"`: User is prompted to approve or deny.
*   (Empty `{}`): Tool proceeds normally (default).

**Important Notes:**
*   **Exit Code Shorthand**: A hook script can exit with code `2` to immediately deny a tool call (equivalent to `permissionDecision: "deny"`).
*   **Timeouts**: The hook execution timeout defaults to 30 seconds.
*   **Payload**: Hook scripts receive the full tool call payload as a JSON object on `stdin`.
*   **Chain of Command**: If multiple hooks are configured for the same event, any hook returning `"ask"` or `"deny"` will take precedence.

## 6. Demo


To see tool confirmation in action, render the demo recording:

<img width="1100" height="600" alt="Image" src="https://github.com/user-attachments/assets/5dff2e3d-f798-485f-a0df-8f17455ddc72" />

```sh
./demos/render.sh tool-confirm
# → demos/out/tool-confirm.gif
```

The demo shows two tool calls: the first is approved, the second is denied.

## 7. Related
*   [Hooks Guide](hooks-guide.md) — Detailed reference for the hook system.
*   [Configuration Guide](configuration-guide.md) — How to manage global and agent-level settings.
