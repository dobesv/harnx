---
title: "Queuing a chat message during an in-flight run in assistant-ui + @ag-ui/client"
date: 2026-07-11
category: integration-issues
problem_type: integration_issue
component: "assistant-ui, @ag-ui/client, web-ui"
root_cause: "assistant-ui's useAgUiRuntime/ag-ui runtime has no built-in user-message queue; submitting while running starts a concurrent runAgent with conflicting AG-UI event streams"
resolution_type: code_fix
severity: high
tags:
  - assistant-ui
  - ag-ui
  - queue
  - concurrency
  - isRunning
  - composer
plan_ref: harnx-web-ui-polish
---

## Problem

Submitting a chat message while an AG-UI run is in progress throws `Cannot send 'TEXT_MESSAGE_CONTENT' event: No active text message found with ID '...'. Start a text message with 'TEXT_MESSAGE_START' first.` The "queued" message is not reliably sent — it is either discarded or errors.

## Symptoms

```text
Error: Cannot send 'TEXT_MESSAGE_CONTENT' event: No active text message found with ID '...'.
       Start a text message with 'TEXT_MESSAGE_START' first.

Behavior: User types message, clicks submit during assistant response → error thrown
Impact: Queued message not sent; may appear as local user message with failed assistant placeholder
Frequency: 100% reproducible when submitting while isRunning=true
```

## Investigation Steps

1. Traced `composerRuntime.send()` in `@assistant-ui/react-ag-ui` — `AgUiThreadRuntimeCore.append()` immediately calls `startRun()` for user messages with no `isRunning` guard.
2. Found single defer path in runtime: `pendingResumeMessageId` for tool-result resume only, not general user-message queue.
3. Inspected `@ag-ui/client` `verify.ts:109-119` — `TEXT_MESSAGE_CONTENT` requires `activeMessages.has(messageId)` from prior `TEXT_MESSAGE_START` in same stream.
4. Confirmed concurrent `runAgent` calls share mutable agent fields; two overlapping AG-UI event streams conflict at verify step.

## Root Cause

assistant-ui's `useAgUiRuntime` / ag-ui runtime has no built-in user-message queue. When `composerRuntime.send()` is called while `isRunning`, a second concurrent `runAgent` starts immediately. The two AG-UI event streams cause conflicts:

1. Second run's `TEXT_MESSAGE_START` races with first run's ongoing stream
2. `@ag-ui/client` verify step expects sequential message IDs within single active stream
3. `TEXT_MESSAGE_CONTENT` for queued message fails verification because its `TEXT_MESSAGE_START` context is corrupted by concurrent stream

## Solution

Implement a UI-level queue that defers `send()` until `isRunning` transitions to idle.

### Pattern

```tsx
function MyThread() {
  const threadRuntime = useThreadRuntime();
  const composerRuntime = useComposerRuntime();
  const [queuedMessage, setQueuedMessage] = useState<string | null>(null);

  // Monitor running state
  const isRunning = useThread(s => s.isRunning);

  // Flush queue when run finishes
  useEffect(() => {
    if (!isRunning && queuedMessage !== null) {
      try {
        composerRuntime.setText(queuedMessage);
        composerRuntime.send();
        setQueuedMessage(null); // only clear on success
      } catch (e) {
        // queue preserved for retry
        console.error('Queue flush failed:', e);
      }
    }
  }, [isRunning, queuedMessage]);

  // Intercept submit while running
  const handleSubmit = () => {
    const text = composerRuntime.text.trim();
    if (!text) return;

    if (isRunning) {
      // Queue for later, don't call send()
      setQueuedMessage(prev => prev ? `${prev}\n${text}` : text);
      composerRuntime.setText(''); // clear composer
    } else {
      composerRuntime.send();
    }
  };

  return (
    <>
      {queuedMessage && <span>1 message queued</span>}
      {/* ... composer with handleSubmit ... */}
    </>
  );
}
```

### Key Implementation Details

1. **Lift queue state**: `queuedMessage` must be accessible to both submit handler and run-finish detector
2. **Clear composer on queue**: User sees immediate feedback; composer empty for next input
3. **Flush on idle**: Effect on `useThread(s => s.isRunning)` triggers when `false`
4. **Wrap flush in try/catch**: Only clear queue state on success; throw preserves draft for retry
5. **Append multiple queues**: If user queues again while already queued, append with newline separator

## Why This Works

**Timing safety**: `composerRuntime.setText()` synchronously mutates composer core's `_text` (base-composer-runtime-core.ts), and `send()` reads current `_text` at call time. Thus `setText → send` ordering is race-free — no async gap between setting text and starting send.

**Run-finish timing**: When `isRunning` effect fires `false`, the runtime has already cleared its running flag before notifying subscribers (`AgUiThreadRuntimeCore.ts:833-837`). Flushing `send()` in that effect is timing-safe.

**Avoids concurrent runs**: By not calling `send()` while running, no second `runAgent` starts. The queued message flows through single event stream after current run completes.

## Prevention Strategies

**Code Review Checklist:**
- [ ] Never call `composerRuntime.send()` while `isRunning` is true
- [ ] Queue state must be React state (not ref) to trigger effect
- [ ] Queue flush must be wrapped in try/catch
- [ ] Only clear queue state on successful `send()`

**Test Cases:**
- Submit message while assistant responding → verify queued message sent after completion
- Submit multiple messages while running → verify all queued messages sent sequentially
- Queue flush throws → verify queue state preserved for retry
- Attachments typed during run → verify documented limitation (not preserved)

**Monitoring:**
- Track queue depth in dev builds
- Warn if concurrent `runAgent` detected (unexpected)

## Known Limitations

**Text-only queue**: This pattern does not preserve attachments across queued sends. Attachments are cleared when composer is cleared. If attachment queue is needed, product decision required for separate implementation.

## Related Issues

- **Plan**: [harnx-web-ui-polish] — UI polish including queue fix
- **Related Solution**: [async-patterns/session-actor-concurrency-invariants-2026-07-04.md](../async-patterns/session-actor-concurrency-invariants-2026-07-04.md) — Backend actor-level prompt queuing (different layer)
- **Related Solution**: [integration-issues/ag-ui-tool-approval-interrupt-resume-2026-07-08.md](./ag-ui-tool-approval-interrupt-resume-2026-07-08.md) — AG-UI interrupt/resume for HITL approval
