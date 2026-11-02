---
harnx: patch
---

Bound the `ToolResults` session-log entry by the broker's `max_payload`: a tool reply that passed the tool server's JSON size check can still overflow its YAML entry and wedge the session forever (#2555). The biggest outputs of an oversized round are degraded first into the same recoverable size error the tool server produces, with the original retained as a `cid:media:` attachment. Orphan repair applies the same bound, so a session already stuck with a journaled oversized reply recovers on its next message. Typed size rejections terminate the current activation on its first failure instead of retrying it ten times, recording an Error entry when it fits. Later user messages can start a fresh activation.

Size-error fallbacks only replace outputs when they shrink and only claim attachment retention when storage succeeded. Shorter messages are tried even when the full error would grow an output, so rounds with many small results can still fit. Metadata lookup failures now warn before degrading without retention.
