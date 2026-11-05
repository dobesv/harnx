---
harnx: patch
---

Fix TUI transcript browse actions (`d` delete, `e` edit, `r` rewind) silently doing nothing on tool-call rows (#2557).

Tool-call rows now carry their session log sequence number both for live sessions and for reloaded/remote sessions, so delete and rewind work on them. Deleting a tool call also removes its paired tool results, including any approval entries journaled between them; rewinding to a tool round keeps the paired tool results (including when approval entries sit between the calls and results) and drops everything after the round (`.rewind` on a tool-calls sequence number no longer errors). Actions that cannot apply to the focused item — editing a non-user message, rewinding or deleting an item without a sequence number — now show a short notice in the footer instead of doing nothing. Delete, edit and rewind are refused with a footer notice while a turn is running (a confirmation opened before the turn started is dropped on confirm too), since mutating history mid-turn races the worker; a failed clipboard copy now shows "Copy failed" instead of a success notice.
