# Run Claude hooks from middleware

`harnx-claude-hook-tools` runs existing Claude-protocol commands, including
rtk/dcg-style input rewriters and command guards. Middleware calls its
`claude_hook_run` tool, applies the returned decision and mutations, then calls
`next`. Claude hooks are an adapter, not the middleware protocol.

## Configure the tool service

Create `tool_servers/claude_hook.yaml`:

```yaml
command: harnx-claude-hook-tools
```

The default server name is `claude_hook`; its tool is `run`. A `--name` override
changes the prefix used in `tools:` and in the `tools.NAME(...)` calls. The
service uses the same implementation for native NATS and MCP stdio. MCP HTTP is
disabled because this tool runs request-supplied host commands without
sandboxing or authentication. In unmanaged/cloud workers, deploy this binary
separately with the worker's NATS endpoint and `HARNX_SERVER_SCOPE`, as with
other independently deployed tool services.
Commands, files and `cwd` refer to the **tool-service host**, not the worker's
BashKit virtual filesystem. Install the hook there.

**This tool can execute arbitrary host programs.** Grant it only to trusted
middleware with `tools: [claude_hook_run]`. Don't include it in the agent's
`use_tools` or `bash_tools`. Keep the command argv in trusted configuration,
never copy it from model-authored tool arguments. NATS scope and session access
are still deployment responsibilities; `payload.session_id` partitions resident
processes but isn't authentication. For stricter isolation, run the service in
a container or expose a dedicated credential/policy tool instead.

## Wrap a tool call

This example runs `dcg` before and after `next`. If your hook only supports
PreToolUse (as many command guards do), remove the post-call block. Replace
`command` with your hook's argv, for example `['rtk', 'rewrite', '--hook']` for
an rtk installation that supports that mode. Tools expecting Claude's `Bash`
name rather than `bash_exec` need an explicit tool-name mapping in `payload`.

```yaml
middleware:
  - name: claude-policy
    match: '^bash_(exec|spawn)$'
    tools: [claude_hook_run]
    timeout: 90s
    python: |
      info = tool_input()
      command = ["dcg"]
      payload = {"session_id": info["session_id"], "tool_name": info["tool"],
                 "tool_use_id": info["tool_call_id"], "tool_input": info["arguments"]}
      pre = saved("claude-pre")
      if pre is None:
          pre = tools.claude_hook_run(command=command, event="PreToolUse", payload=payload)
          save("claude-pre", pre)
      if pre.get("is_error") or pre.get("isError"):
          deny("Claude pre-hook failed: " + str(pre))
      elif pre["decision"] == "deny":
          deny(pre["reason"] or "Denied by Claude hook")
      elif pre["decision"] == "ask" and not confirm(pre["reason"] or "Run this tool?", id="claude-pre"):
          deny("Claude hook request denied")
      else:
          arguments = pre["tool_input"] if pre["tool_input"] is not None else info["arguments"]
          result = saved("claude-downstream")
          if result is None:
              result = call_next(arguments, id="claude-run")
              save("claude-downstream", result)
          payload["tool_input"] = arguments
          payload["tool_response"] = result["output"]
          event = "PostToolUseFailure" if result["is_error"] else "PostToolUse"
          if result["is_error"]:
              payload["error"] = str(result["output"])
          post = saved("claude-post")
          if post is None:
              post = tools.claude_hook_run(command=command, event=event, payload=payload)
              save("claude-post", post)
          if post.get("is_error") or post.get("isError"):
              deny("Claude post-hook failed. Tool already ran: " + str(post))
          elif post["decision"] == "deny":
              deny("Tool already ran: " + (post["reason"] or "Result denied by Claude hook"))
          elif post["decision"] == "ask" and not confirm(post["reason"] or "Keep this result?", id="claude-post"):
              deny("Result discarded. Tool already ran.")
          else:
              if post["tool_response"] is not None:
                  result["output"] = post["tool_response"]
              context = pre["context"] + post["context"]
              if context:
                  result["output"] = {"result": result["output"], "claude_hook_context": context}
              respond(result)
```

`save`/`saved` preserve the hook replies and downstream result across a durable
confirmation. They don't cache ordinary future calls. A post-call denial
cannot undo the operation. The example keeps the downstream result envelope's
`ref`, so private content and metadata still come from that dispatch. Existing
error markers cannot be cleared by a post-hook mutation.

There is no middleware `add_context` operation. This example puts context in
the effective tool output so the model sees it; that deliberately wraps the
output in `{result, claude_hook_context}`. Remove or adapt that block if your
tool has a required output shape. Lifecycle event hooks have a separate context
channel. Helper tool calls bypass middleware, preventing recursion even if the
match regex includes `claude_hook_run`.

## Tool API

`claude_hook_run` accepts:

| Field | Meaning |
| --- | --- |
| `command` | Required program/argv array. No implicit shell or variable expansion. Use `['sh', '-c', '...']` only when a shell is intended. |
| `event` | Required Claude event name, such as `PreToolUse`, `PostToolUse`, or `PostToolUseFailure`. Lifecycle names are supported too. |
| `payload` | Required object containing Claude event fields (`session_id`, `tool_name`, `tool_input`, `tool_use_id`, plus `tool_response`/`error` for post events). Extra fields are preserved. `hook_event_name` is inserted from `event`; conflicting names and `id` are rejected. |
| `persistent` | Default false. Reuse a JSONL subprocess instead of starting a command per request. |
| `timeout_secs` | Total deadline, 1–300 seconds, default 30. Includes process queue, spawn, stdin and output. The middleware's inclusive timeout applies too. |
| `package_dir` | Optional directory injected as `HARNX_PACKAGE_DIR`. |

If `payload.cwd` is supplied, it must be absolute and exist on the service
host. Both one-shot and resident commands start there. If omitted, the service's
working directory is used. Environment is inherited from the service;
`HARNX_HOOK_PROTOCOL` is unset for one-shot commands and set to `jsonl` for
resident commands.

Successful calls return an object with all of these fields:

- `decision`: `allow`, `deny`, or `ask`. The tool doesn't enforce this decision.
- `reason`: string or null. `ask` must become a middleware `confirm`, not an
  unconditional allow.
- `tool_input`: replacement arguments object or null. Recognizes top-level
  `mutatedToolInput`, then `hookSpecificOutput.updatedInput`, then legacy
  `hookSpecificOutput.toolInput`, in that order. It doesn't merge arguments.
- `tool_response`: replacement output or null. Same precedence for
  `mutatedToolResponse`, `updatedMCPToolOutput`, and legacy `toolResponse`.
- `context`: strings from top-level `additionalContext`, nested
  `hookSpecificOutput.additionalContext`, then `systemMessage`.
- `result`: original Claude response, including fields used by lifecycle hooks.

`hookSpecificOutput.permissionDecision` supplies the decision. Legacy
`decision: block` (or `deny`) and `continue: false` also deny. Top-level
`approve`/`allow` and `ask` decisions are accepted; unknown decisions are errors.
One-shot exit code 2 denies with stderr as the reason. Empty stdout allows;
plain-text stdout becomes context. Other nonzero exits, invalid JSON-looking output, spawn failures,
invalid responses and timeouts are tool errors, **not** implicit permission.
Check `is_error`/`isError` before inspecting a tool reply, as in the example.
Unknown permission decisions are errors.

## Persistent JSONL commands

Set `persistent: true` and include your hook's own `--persistent` argument if
it requires one. Each stdin line is the flattened Claude payload plus a fresh
`id`. Each stdout response must be one JSON object with the same `id` and
Claude result fields at the top level, not nested under `result`. Flush every
response. Standalone `notice` lines are consumed but don't make decisions or
request interactions; return user/model context in the response instead.

The service caches at most 32 resident processes, keyed by argv, working
directory, package directory and payload session ID. Requests to each process
are serialized; different keys run independently. Idle slots may be evicted.
Don't rely on resident memory surviving service restarts, eviction or a failed
exchange. A failed exchange retires that process; the service never retries
the request automatically.

Requests and stdout are limited to 256 KiB each (JSONL replies and notices share
that budget per request). Stderr is drained continuously with only its last
4 KiB retained for diagnostics. Cancellation, timeout or dropping an exchange
retires its process. Unix process groups receive best-effort cleanup; detached
descendants and Windows child trees aren't a sandbox guarantee. One-shot hooks
must close their stdout/stderr pipes; background descendants that inherit them
can keep the call open until its deadline.

Hook commands may have side effects. This tool advertises neither read-only nor
idempotent execution. Middleware helper dispatches use the existing durable
journal and unknown-outcome rules. A crash after a hook ran but before a durable
confirmation boundary can end the outer call as interrupted rather than replay
it. See [middleware recovery](middleware-guide.md#crash-recovery-and-unknown-outcomes).
