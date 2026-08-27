# Tool Confirmation Guide

Tool confirmation allows you to inspect and approve tool calls before they execute. This provides a safety layer for destructive operations, an audit trail for sensitive actions, and a way to learn how the agent interacts with your system.

## 1. Quick Start

The fastest way to enable manual confirmation for all tools is to add an
embedded jaq hook to your `config.yaml`:

```yaml
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --jaq
        '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Manual approval required"}}'
```

With this configuration, Harnx will pause and prompt you for every tool call.
The expression uses jq syntax but is evaluated by Harnx's embedded jaq engine,
so no `jq` or `jaq` executable is required.

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

### Method A: Selective Confirmation (Matcher)

Use the `--matcher` flag to require confirmation only for specific tools:

```yaml
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --matcher '^(bash_exec|bash_spawn)$'
        --jaq
        '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Shell command requires approval"}}'
```

*The matcher uses a regex against the tool name.*

### Method B: Per-Agent Hooks
Enable confirmation only for specific agents by adding the hook to their Markdown front-matter:

```yaml
---
model: openai:gpt-4o
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --jaq
        '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Manual approval required"}}'
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

Embedded jaq hooks receive the full event payload. This hook asks only when a
shell command contains a potentially destructive command name:

```yaml
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --matcher '^(bash_exec|bash_spawn)$'
        --jaq
        'if ((.tool_input.command // "") | test("\\b(rm|mv|cp|chmod|chown|dd|mkfs)\\b"))
         then {"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Command uses a potentially destructive operation"}}
         else {} end'
```

The expression must return an object in the normal hook response shape. Return
`{}` when no action is needed. Use an external command hook when the policy
needs I/O or logic that is not practical in jaq.

### Confirming an Agent Handoff

Agent handoffs are tool calls, so an exact matcher can make a handoff require
approval without affecting other tools. For example, this requires confirmation
before Daedalus hands a session to Atlas:

```yaml
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --matcher '^atlas_session_handoff$'
        --jaq
        '{"hookSpecificOutput":{"permissionDecision":"ask","permissionDecisionReason":"Hand off this plan to Atlas for execution?"}}'
```

The bundled Pantheon `daedalus` agent includes this hook by default.

### Permission Decision Values
Hooks can return these values in `hookSpecificOutput`:
*   `"allow"`: Tool proceeds without prompting.
*   `"deny"`: Tool is blocked immediately (agent gets error).
*   `"ask"`: User is prompted to approve or deny.
*   (Empty `{}`): Tool proceeds normally (default).

**Important Notes:**
*   **Exit Code Shorthand**: A hook script can exit with code `2` to immediately deny a tool call (equivalent to `permissionDecision: "deny"`).
*   **Timeouts**: The hook execution timeout defaults to 30 seconds.
*   **Payload**: Jaq expressions receive the payload as input. External hook commands receive the same JSON object on `stdin`.
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
