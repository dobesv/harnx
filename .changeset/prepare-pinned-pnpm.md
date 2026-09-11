---
harnx: patch
---

Make `cargo xtask install` prepare the pnpm version pinned by the web project
with Corepack, and run every pnpm command from `web/` so Corepack always finds
that version before building the web UI.
