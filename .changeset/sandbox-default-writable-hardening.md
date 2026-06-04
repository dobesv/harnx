---
harnx: minor
---

Harden the sandbox default whitelist: directories on `PATH` or holding host-executed binaries (`~/.nvm`, `~/.cargo/bin`, `~/.pyenv`, `~/.rye`, `~/.mono`, `~/.local/share/{claude,opencode,pipx}`) are now **read+execute only**, and package-manager caches (`~/.npm`, `~/.yarn`, `~/.cargo/registry`, `~/.cargo/git`, `~/.bun/install/cache`, `~/.local/share/{pnpm,uv}`) are **read+write only**. No `$HOME` directory is granted write+execute by default. This closes a sandbox-escape vector where a compromised sandboxed process could plant a malicious executable in a writable directory that the user later runs on the host.

Privileged operations that install or self-update executables (`cargo install`, `nvm install`, `pyenv install`, `rye sync`, `pipx install`, `claude update`, `opencode` self-update) now require explicit write access — pass `--extra-rwx <path>` (or set `HARNX_BASH_EXTRA_RWX` for the bash MCP server), or run them outside the sandbox.

A custom `CARGO_HOME` now also receives the same defaults as the standard `~/.cargo`: read access to its root (for `config.toml`/credentials), read+exec for `bin`, and read+write for the `registry` and `git` download caches.
