---
harnx: patch
---
Bootstrap pnpm's native executable through Corepack before using a sandboxed pnpm
shim during `cargo xtask install`, preventing read-only cache errors after pnpm
version updates.
