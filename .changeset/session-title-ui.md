---
harnx: minor
---

feat(ui): show the generated session title in the terminal and browser tab

The automatically generated session title now sets the terminal window title in
the TUI and the browser tab title in the web UI (as `harnx — <title>`), updating
live as the title is (re)generated or set with `.set title`. Adds an
`example-title-agent` and `title_agent` / `title_update_threshold` settings to
the example configuration.
