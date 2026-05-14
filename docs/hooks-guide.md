# Hooks Guide

Hooks allow you to extend Harnx by running external commands at specific points during agent execution. They can be used for auditing, policy enforcement, environment injection, or even modifying the interaction between the agent and its tools.

## 1. Overview

Hooks are external programs or scripts that Harnx executes when specific events occur. Depending on the event and the hook configuration, a hook can:

*   **Observe**: Record events for logging or auditing without affecting execution.
*   **Block**: Prevent a tool from running or stop an agent turn.
*   **Ask**: Pause execution and request user confirmation.
*   **Mutate**: Modify tool arguments before execution or tool responses after execution.
*   **Resume**: Trigger an additional agent turn after the agent has stopped.

## 2. Configuration

Hooks are configured in the `hooks` section of the global `config.yaml` or within an agent's front-matter.

### Configuration Fields

| Field | Type | Description |
| :--- | :--- | :--- |
| `event` | string | **Required.** The event that triggers the hook (e.g., `PreToolUse`). |
| `type` | string | **Required.** The execution protocol: `claude-command` or `claude-command-persistent`. |
| `matcher` | string | **Optional.** A regex matched against the `tool_name` for tool-related events. |
| `command` | string | **Required.** The shell command or path to the executable to run. |
| `timeout` | integer | **Optional.** Execution timeout in seconds (default: 30). |
| `status_message` | string | **Optional.** A message displayed to the user while the hook is running. |
| `async` | boolean | **Optional.** If `true`, the hook runs in the background. Async hooks cannot block or mutate. |
| `max_resume` | integer | **Optional.** (Top-level only) Maximum number of times a `Stop` hook can request a resume. |

### Hook Location and Merging

Hooks can be defined globally in `config.yaml` or per-agent in the agent's front-matter.

*   **Global Hooks**: Apply to all agents and sessions.
*   **Agent Hooks**: Defined in an agent's YAML front-matter.
*   **Merging**: Agent hooks extend the global list. If an agent hook has the same `event` and `matcher` as a global hook, the agent hook **replaces** the global one.
*   **max_resume**: If set in an agent's front-matter, it overrides the global `max_resume` value.

## 3. Event Reference

Harnx supports the following events. Each event sends a JSON payload to the hook.

| Event | When it fires | Payload Fields | Capabilities |
| :--- | :--- | :--- | :--- |
| `SessionStart` | At the beginning of a session. | `session_id`, `cwd`, `source`, `model` | Observe |
| `SessionEnd` | When a session terminates. | `session_id`, `cwd`, `reason` | Observe |
| `UserPromptSubmit` | When the user sends a prompt. | `session_id`, `cwd`, `prompt` | Observe |
| `Stop` | When the agent finishes its turn. | `session_id`, `cwd`, `stop_hook_active`, `last_assistant_message` | Resume |
| `StopFailure` | When an agent turn fails. | `session_id`, `cwd`, `error`, `error_type` | Observe |
| `PreToolUse` | Before a tool is executed. | `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_use_id` | Block, Ask, Mutate |
| `PostToolUse` | After a tool successfully runs. | `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_response`, `tool_use_id` | Mutate |
| `PostToolUseFailure`| When a tool execution fails. | `session_id`, `cwd`, `tool_name`, `tool_input`, `tool_use_id`, `error` | Observe |

## 4. Hook Types

### `claude-command` (One-shot)

The most common hook type. Harnx spawns the command as a subprocess for every event match.
*   **Input**: The event payload is sent to the command's `stdin` as a single JSON object.
*   **Output**: Harnx reads `stdout` for a JSON response (see Protocol below).
*   **Control**:
    *   Exit code `0`: Continue execution.
    *   Exit code `2`: Block execution (equivalent to `permissionDecision: "deny"`).
    *   Other non-zero codes: Logged as errors, but execution usually continues.

### `claude-command-persistent` (Persistent)

Useful for hooks that need to maintain state or have high startup overhead. The process is started at the beginning of the session and kept alive.
*   **Protocol**: JSONL (JSON Lines) over `stdin` and `stdout`.
*   **Correlation**: Each request from Harnx includes a unique `id` field. The hook must include the same `id` in its response line.

### Async Hooks (`async: true`)

Any hook can be marked as `async`.
*   Harnx fires the hook and continues immediately without waiting for a response.
*   Async hooks **cannot** block or mutate tool data. Any such fields in their response are ignored.
*   They can still request a `resume` or provide `additionalContext`, which will be applied to the *next* agent turn.
*   Harnx ensures all async hooks have completed their execution before starting a new LLM turn.

## 5. Response Protocol

Hooks communicate their results by printing a JSON object to `stdout`.

```json
{
  "additionalContext": "Text to append to the agent's conversation history",
  "resume": false,
  "systemMessage": "A hidden message to inject into the system prompt for the next turn",
  "hookSpecificOutput": {
    "permissionDecision": "allow",
    "permissionDecisionReason": "Reason for the decision",
    "toolInput": { "arg": "mutated value" },
    "toolResponse": { "result": "mutated response" }
  }
}
```

### Field Details

*   **`additionalContext`**: String. Appended to the conversation context.
*   **`resume`**: Boolean. If `true`, requests that the agent perform another turn (primary use for `Stop` hooks).
*   **`systemMessage`**: String. Injected as a system-level message.
*   **`hookSpecificOutput`**:
    *   **`permissionDecision`**: One of `"allow"`, `"deny"`, or `"ask"`.
    *   **`permissionDecisionReason`**: Description of why access was allowed, denied, or why the user is being asked.
    *   **`toolInput`**: (PreToolUse only) An object that replaces the original arguments sent to the tool.
    *   **`toolResponse`**: (PostToolUse only) A value (object or string) that replaces the original response from the tool.

## 6. Mutation

Mutation allows hooks to transparently modify the data flowing between the agent and its tools.

### Chaining Semantics

When multiple hooks are configured for the same event (e.g., two `PreToolUse` hooks for `bash_exec`):
1.  Hooks fire in **document order** (global hooks first, then agent hooks).
2.  Each hook receives the **current** value in its payload. If a previous hook mutated the value, the subsequent hook sees the mutated version.
3.  The final value used by Harnx is the one produced by the **last** hook in the chain that provided a mutation.

### Mutation Example: Injecting Environment Variables

A `PreToolUse` hook can inject secrets or configuration into a `bash_exec` call.

**hook-script.sh:**
```bash
#!/bin/bash
# Read payload from stdin
PAYLOAD=$(cat)
TOOL_INPUT=$(echo "$PAYLOAD" | jq '.tool_input')

# Inject AWS credentials into the command
NEW_COMMAND="AWS_ACCESS_KEY_ID=xxx AWS_SECRET_ACCESS_KEY=yyy $(echo "$TOOL_INPUT" | jq -r '.command')"
NEW_INPUT=$(echo "$TOOL_INPUT" | jq --arg cmd "$NEW_COMMAND" '.command = $cmd')

# Return the mutated input
echo "{\"hookSpecificOutput\": {\"toolInput\": $NEW_INPUT}}"
```

## 7. Permissions

The `permissionDecision` field controls whether a tool is allowed to run.

*   **`allow`**: Execution continues.
*   **`deny`**: Execution is blocked. The agent receives a message indicating the tool was blocked.
*   **`ask`**: Harnx prompts the user to allow or deny the execution. The `permissionDecisionReason` is shown to the user.

**Shorthand**: A hook script can exit with code `2` to immediately signal a `deny` decision without printing a JSON response.

## 8. Resume

The `resume` field is primarily used with the `Stop` event. It allows a hook to decide if the agent should continue thinking or performing tasks.

*   **Loop Protection**: To prevent infinite loops, the `max_resume` setting (default: 3) limits how many consecutive resumes can be triggered.
*   **Async Drain**: Harnx waits for all pending async hooks to finish before checking if a resume was requested. This ensures that any context gathered by background tasks is available to the agent in the next turn.

## 9. Persistent Hooks

Persistent hooks (`type: claude-command-persistent`) use a JSONL-based protocol to avoid the overhead of spawning a new process for every event.

### Request Format (Harnx to Hook)
```json
{"id": "1", "session_id": "...", "cwd": "...", "hook_event_name": "PreToolUse", ...}
```

### Response Format (Hook to Harnx)
```json
{"id": "1", "additionalContext": "...", "hookSpecificOutput": {...}}
```

The hook process must read one line from `stdin`, process it, and write exactly one line to `stdout`. The `id` must match the request.

## 10. Examples

### 1. AWS Credential Injector (PreToolUse Mutation)

Injects AWS credentials into any `bash_exec` call.

```yaml
# config.yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command
      matcher: bash_exec
      command: "inject-aws-creds.sh"
```

**inject-aws-creds.sh:**
```bash
#!/bin/bash
# Replaces command with one prefixed with AWS env vars
input=$(jq -r '.tool_input.command')
new_input=$(jq -n --arg cmd "AWS_REGION=us-east-1 $input" '{"command": $cmd}')
echo "{\"hookSpecificOutput\": {\"toolInput\": $new_input}}"
```

### 2. GitHub Auth Injector (PreToolUse Mutation)

Intercepts GitHub API calls or git commands to inject a token.

```bash
#!/bin/bash
input=$(cat)
tool_name=$(echo "$input" | jq -r '.tool_name')

if [[ "$tool_name" == "git_clone" ]]; then
  url=$(echo "$input" | jq -r '.tool_input.url')
  # Rewrite https://github.com/... to https://<token>@github.com/...
  new_url=${url/https:\/\/github.com/https://$GH_TOKEN@github.com}
  echo "{\"hookSpecificOutput\": {\"toolInput\": {\"url\": \"$new_url\"}}}"
else
  echo "{}"
fi
```

### 3. Tool Deny-list (Block Tools)

Prevents the use of `bash_exec` for security reasons.

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command
      matcher: bash_exec
      command: "exit 2"
      status_message: "Blocking bash_exec for safety"
```

### 4. Audit Logger (Async PostToolUse)

Logs all tool results to a file in the background.

```yaml
hooks:
  entries:
    - event: PostToolUse
      type: claude-command
      command: "tee -a tool_audit.log"
      async: true
```
