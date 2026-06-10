---
harnx: patch
---
Fix `cargo xtask install` failing with "Text file busy" (ETXTBSY) when a target binary is currently running. The installer now copies to a temp file and atomically renames it over the destination, matching the old `cp -f` behaviour so install works without stopping existing harnx processes.
