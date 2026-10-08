---
harnx: patch
---

`harnx-sandbox-run` now grants a path that is, or goes through, a symlink both as given and as it resolves, so it works by either name inside the sandbox. Before, only the resolved path was available on Linux. That also made `--no-defaults` fail on merged-/usr systems, where binaries reach their ELF interpreter through the `/lib64` symlink.
