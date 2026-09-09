---
harnx: patch
---

Show token usage once per completed turn instead of after every model call. The turn-end usage line now sums the whole tool loop, renders on its own line in the CLI (no longer appended to streamed text), and uses the same 📥/📤/💾 format as the status bar. Removes the inconsistent inline per-call usage lines in both the TUI and CLI.
