---
harnx: minor
---
Add GFM markdown table rendering to TUI (as native Ratatui Table widget) and terminal outputs. Replace `tui-markdown` with a custom `pulldown-cmark` renderer producing composite Ratatui widgets. Add per-item render cache to eliminate O(N) per-frame parse cost on long transcripts.
