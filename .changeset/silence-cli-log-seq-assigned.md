---
harnx: patch
---

Stop the non-interactive CLI from printing raw `[event] LogSeqAssigned { seq: N }` debug lines on stderr during `prompt` runs.

`SessionEvent::LogSeqAssigned` is persistence bookkeeping: the TUI patches transcript rows with the assigned log sequence so edit/delete/rewind can target the right entry, but the CLI makes no use of it. It had no explicit match arm in the CLI event sink, so it fell through to the `[event] {other:?}` debug catch-all and printed once per log write. It's now dropped silently, matching how the sink already ignores other internal-only events.
