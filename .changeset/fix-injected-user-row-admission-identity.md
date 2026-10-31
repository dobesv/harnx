---
harnx: patch
---

Fixed running tasks failing with "injected user row has no admission identity" when a mid-turn compaction re-logged injected prompts or runtime notes.

Messages queued during a running task now keep their admission identity when carried into the model's context, and replayed runtime notes (sub-agent starts and interrupt notices) are marked as non-executable context, so compacting the session mid-turn can no longer turn them into prompts the worker refuses to run. Admission verification now checks exactly the messages queued for injection instead of re-scanning history, which also stops old compaction copies from being mistaken for competing input, and the injection cursor no longer re-delivers messages after an edit re-sequences the log. Prompt admission now rejects caller-supplied message ids that use the reserved `runtime-note-v1-` and `timing-v1-` prefixes on every admission path — ordinary prompts, tool confirmations and fixed-ticket admission — so a client prompt can never be filed as runtime context. Fixed-admission invocation ids are checked against the same reserved prefixes, because A2A republish appends the prompt under that id; and a reserved id that already names a durable runtime-note row is rejected instead of being returned as an existing prompt.

Clearing a session (`/empty` on a live session, or replaying a `clear` entry) now also drops the stored compaction summary, so the next model request no longer includes a summary of conversation history that no longer exists.

**Recovery for sessions already hit by the old bug:** a session containing a broken ID-less row still fails that activation, but is not wedged. The failed activation durably records an `Error` entry (unless the process lost its lease mid-failure), which terminates the turn that the broken row was pending for. Submitting a new prompt then starts a clean turn and the session works again. Only if no terminator was written at all (lease loss, append failure, hard process death) do you need to cancel the stuck turn or rewind the session before continuing. There is no automatic text-based repair: prefix lookalikes are deliberately kept executable.
