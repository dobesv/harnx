---
harnx: minor
---
Auto-whitelist Go build caches and support arbitrary `$VAR` expansion in sandbox whitelist paths.

- The bash sandbox now grants read+write (but not execute) access to `GOMODCACHE` and `GOCACHE` when those environment variables are set, and forwards both to the sandboxed process. This fixes `go build`/`go test` failing with `read-only file system` when a custom cache location is configured. Caches hold source, `.a` archives, and build logs only — no executables — so execute access is intentionally withheld.
- Sandbox whitelist arguments (`--extra-read`/`--extra-write`/`--extra-exec`/`--extra-rwx`) now expand arbitrary `$VAR` references from the environment in addition to the existing pseudo-vars (`$GIT_ROOT`, etc.). A leading `$NAME` or `$NAME/...` resolves to the environment value; unset variables are left literal. Pseudo-vars still take precedence. The home-directory exposure guard continues to apply at the call sites.
- Deduplicated the Go/toolchain default-path logic so `harnx-sandbox-run` and `harnx-sandbox-common` share one implementation. As part of this, the `.exists()` gating was dropped for toolchain env-relative paths (`CARGO_HOME`/`GOROOT`/`GOPATH`/`GOBIN` and the new cache vars): they are now whitelisted unconditionally when the variable is set, since cache directories often don't exist on first run.
