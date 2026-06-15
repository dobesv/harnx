---
harnx: patch
---
Simplify TUI streamed-assistant-text accumulation. Streamed text now coalesces into a single transcript block per unbroken run; an interleaving item (tool call, tool result, notice, source heading) ends the run so the following text starts a fresh block below it. This replaces the previous per-line splitting and the index-based bookkeeping (`streaming_assistant_idx`) with a single open/closed flag and a "look at the trailing item" rule, removing a fragile multi-branch loop.
