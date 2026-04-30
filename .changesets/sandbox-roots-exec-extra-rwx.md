---
harnx: minor
---

Improve filesystem sandboxing in `harnx-mcp-bash`:

- Sandbox roots are now automatically executable. This fixes issues when running compilers like `cargo build` and loading native extensions (e.g., Python `.so` files or Rust proc-macros) that are built inside the project tree.
- Add `--extra-rwx <path>` CLI flag and `HARNX_BASH_EXTRA_RWX` environment variable for granting read, write, and execute permissions to a path outside of roots (e.g., `~/.cargo`).
