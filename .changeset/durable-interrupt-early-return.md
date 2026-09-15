---
harnx: patch
---

Return TUI and one-shot prompt control as soon as interruption is durably accepted. New generations can run while prior tool and model cleanup continues in the background; late replies and confirmation requests remain fenced to their original generation. Remote AG-UI streams also end on accepted interruption without waiting for lease cleanup, and cannot forward a replacement generation's events under the interrupted run ID.

Esc restores the TUI editor during unresolved cancellation without discarding its request or draft. Submission stays guarded until acceptance; known-generation failures can be abandoned directly without a second confirmation modal. Retries keep the original generation and editor state.
