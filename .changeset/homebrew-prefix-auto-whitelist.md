---
harnx: minor
---
Auto-whitelist the Homebrew install prefix in the default sandbox allowlist so Homebrew-managed tools work without manual overrides.

- The Homebrew prefix is granted read+execute (never write) by default. The location is resolved dynamically: `HOMEBREW_PREFIX` is honoured when set, otherwise a compile-time platform default is used (`/opt/homebrew` on macOS, `/home/linuxbrew/.linuxbrew` on Linux). No runtime OS detection is performed.
- Fix a `/usr/local` static-path oversight: `/usr/local` is now readable on both Linux and macOS, and `/usr/local/lib` is now executable on Linux (macOS already had it) so dynamically linked binaries can load their dylibs (#818).
