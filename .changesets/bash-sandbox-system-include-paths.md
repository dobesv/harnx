---
harnx: minor
---
`harnx-mcp-bash` now grants sensible default sandbox access for common developer toolchains so that `cargo build`, `npm install`, `pip install` (with native extensions), and similar commands work without manually configuring `HARNX_BASH_EXTRA_*` for every project. New defaults:

- **Read** (Linux): `/usr/include`, `/usr/include/x86_64-linux-gnu` — fixes builds of crates with C bindings such as `onig_sys`.
- **Read** (under `$HOME`): `~/.gitconfig`, `~/.gitignore`, `~/.gitignore_global`, `~/.tool-versions`, `~/.local`.
- **Exec** (under `$HOME`): `~/.local/bin`, `~/.local/lib`, `~/.bun`, `~/.asdf`, `~/go/bin`.
- **Read+Write** (under `$HOME`): `~/.cache`, `~/go/pkg`.
- **RWX** (under `$HOME`): `~/.npm`, `~/.yarn`, `~/.nvm`, `~/.cargo`, `~/.mono`, `~/.bun/install/cache`, `~/.pyenv`, `~/.rye`.

Toolchain-locating environment variables are now honoured automatically: `CARGO_HOME` adds `$CARGO_HOME/bin` (exec), `GOROOT` adds `$GOROOT` (exec), `GOPATH` adds `$GOPATH/bin` (exec) and `$GOPATH/pkg` (read+write), and `GOBIN` adds `$GOBIN` (exec). Non-existent paths are silently skipped by the sandbox helper.
