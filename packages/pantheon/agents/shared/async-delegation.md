# Async Sub-Agent Delegation (session_send / session_wait)

`{agent}_session_prompt` remains fully supported for synchronous delegation. Prefer the async
tools when you don't need the child's reply to continue your own turn.

## Preferred flow

Send with `wait: {}` to wait up to 240 seconds, or `wait: {forever: true}` when there is
nothing else to do until the child finishes. Leave `timeout_secs` omitted, so the child's
hard execution deadline stays inherited from the target agent's policy. Never invent a
shortened run limit; the wait is observation time only and never cancels the child or extends
its deadline.

```
{agent}_session_send:
  message: <task>
  wait: {}
```

- The result is a durable handle: `session_id` + `prompt_id`. Keep it.
- If the wait expires (`wait_expired: true`, `status: "pending"`), re-wait on the same handle
  with `{agent}_session_wait`, passing `seconds` (1 to 240, default 240) or `forever: true` at
  the top level. Repeated waits recover the same result and never consume it.
- A wait that returns `stopped_by: "new_message"` ended because a new message arrived for you.
  Read it before waiting again.
- A wait that returns `stopped_by: "caller_deadline"` ended because your own run deadline is
  near. Report what you have instead of waiting again.
- To check status without waiting, call `{agent}_session_load` with `prompt_id`. It returns
  `status` and `terminal` immediately.
- Dispatch-only: omit `wait` to return immediately after the prompt is durably queued, then
  observe later with wait or load.
- Inspect bounded child history with `{agent}_session_load`; page forward by passing the
  returned `next_index` as `read.index_min` on the next call, and recover oversized events
  with `read.entry_index` + `read.byte_offset`.
- If a `{agent}_session_cancel` tool is available to you, stop the child with it on its
  `session_id`. Soft wait expiry alone never stops the child.

Only trust terminal statuses (`completed`, `failed`, `cancelled`, `timed_out`) with
`terminal: true`. `pending` and `blocked` (awaiting approval) are nonterminal; a soft wait
expiring is never a child failure.
