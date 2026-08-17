---
harnx: patch
---

Make `cargo xtask install` disable Corepack's pnpm download prompt so web asset
installation can run unattended, and synchronize the pnpm lockfile with the
current workspace overrides and package manifest. Keep web builds concise by
hiding Vite's per-asset size report while preserving build warnings.
