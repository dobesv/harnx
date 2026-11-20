---
harnx: patch
---
Keep delegates dispatched with `session_send` running after parent cancellation or failure, including cancellation during inline waits and server restart recovery. Cancelling `session_send` or `session_wait` stops observation only; explicit `session_cancel` and inherited run deadlines still stop the child. Synchronous `session_prompt` and `session_new` cancellation behavior is unchanged. Only work admitted after the upgrade is detached, and the guarantee needs every worker on the new version: an older worker that resumes a detached child after its parent stopped still cancels it.
