---
harnx: minor
---
Add standalone `harnx-serve` and `harnx-acp-server` binaries. `cargo install harnx-serve` ships an HTTP-only server (~10 MB release, no TUI deps) and `cargo install harnx-acp-server` ships an ACP-only agent (~11 MB). Headless deployments can skip ratatui/crossterm entirely. The full `harnx` binary keeps all four modes (TUI/Cmd/Serve/Acp) as before.
