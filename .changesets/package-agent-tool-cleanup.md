---
harnx: patch
---
Package agents (pantheon, coding) now list individual tool names instead of the `fs_read_tools` / `fs_write_tools` toolset aliases (which were only defined in `example_config/config.yaml`) and the `bash_*` wildcard, so they work regardless of the user's own `toolsets:` configuration. Pytheas and Zosimus now explicitly default to the current working directory as the repository under investigation rather than asking the user.
