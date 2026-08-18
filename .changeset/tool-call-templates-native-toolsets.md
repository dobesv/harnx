---
harnx: patch
---
Restore the custom markdown rendering of tool calls for the bash, fs, plans, time, and sub-agent tool servers. Their native toolsets never advertised the display templates, so the TUI fell back to a generic YAML dump of the arguments.
