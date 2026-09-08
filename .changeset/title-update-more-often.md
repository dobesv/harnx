---
harnx: minor
---
Session titles now regenerate during the tool-call loop in addition to turn end, triggered by token growth (`title_update_threshold`) or an optional time interval (`title_update_interval_secs`). The title agent also sees the current turn's in-progress thinking and tool calls.
