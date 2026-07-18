---
"harnx": minor
---

`cargo xtask install` now builds the web UI (`pnpm install` + `pnpm build` in
`web/`) and copies the compiled assets into the default directory `harnx serve`
loads from (`<data_dir>/web-assets`, e.g. `~/.local/share/harnx/web-assets`), so
a local install serves the web client out of the box. Pass `--skip-web` to
install only the Rust binaries. Closes #1040.
