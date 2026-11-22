---
harnx: patch
---
Script mode: optional tool flags take plain values, tool commands no longer print user-facing summary lines, and the `bash` result reaches the model once instead of twice. `harnx call bash` now prints the script's stdout and stderr and exits with its status, like `bash -c`; pass `--json` for the full result.
