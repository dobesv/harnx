# Tool Confirmation Guide

Tool confirmation pauses a tool call so you can inspect its full arguments, approve it, or return a blocked result to the agent.

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

Harnx uses the hooks system for tool confirmation. When a `PreToolUse` hook returns `{"permissionDecision": "ask"}`, this sequence runs:

1. The model requests a tool such as `bash_exec`.
2. The worker runs matching `PreToolUse` hooks.
3. The worker sends a confirmation request over NATS to the attached TUI.
4. The TUI shows the tool name, full arguments, hook reason, and optional message area.
5. Approving runs the tool. Rejecting returns a synthetic blocked tool result so the agent can continue.

Production confirmation always uses this worker-to-frontend NATS route. The old test-only local confirmation bridge has been removed.

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

## 4. Use the TUI Confirmation Modal

The modal shows full tool arguments as multiline YAML. If the tool declares a transcript call template, the modal shows that rendered template first; press `Ctrl+F` to switch between the template and raw YAML.

The modal grows to the available screen height. Use `PageUp`, `PageDown`, or the mouse wheel to scroll the tool-call body. The header, reason, optional message area, and key help remain visible.

The message area is focused while the modal is open:

- Type an optional message for the agent.
- Press `Shift+Enter` or `Alt+Enter` to insert a newline.
- Press `Enter` to approve after the keyboard has been idle for two seconds. An early `Enter` is ignored and does not restart the idle timer.
- Press `Ctrl+D` to reject the tool. The agent continues with a blocked tool result.
- Press `Ctrl+C` to reject and interrupt the session. Text from the message area returns to the main input as a draft and is not queued.
- Press `Ctrl+F` to switch between template and raw YAML when a template is available.

On approval or `Ctrl+D`, a non-empty optional message is durably queued to the originating session before the TUI sends the confirmation reply. The agent receives the real or blocked tool result first, then the queued user message. An empty message sends only the approval or rejection.

TUI confirmation prompts have no time limit. Cancelling the turn, closing the route, or detaching the TUI ends the wait. If message enqueue fails, the modal stays open with the draft intact so you can retry.

If no TUI is attached, a tool call requiring confirmation is denied. A rejected tool call produces a result such as `{"error": "Denied by user", "blocked_by_hook": true}`.

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
