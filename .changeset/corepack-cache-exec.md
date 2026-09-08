---
harnx: patch
---
Package managers launched through Corepack now run inside the sandbox without extra flags. `~/.cache/node/corepack` is granted exec by default.

Corepack spawns the pinned package manager straight out of that cache. Through pnpm 11 it spawned a script and ran it via `node`, which the existing exec grant on `node` covered. pnpm 12 ships a native binary instead, and `~/.cache` is a read+write default with no exec, so the download succeeded and the spawn failed with `Could not run the pnpm binary at ~/.cache/node/corepack/v1/pnpm/<version>/pnpm-native: EACCES`. Until now the only way past it was to patch your own shim.

The cache is listed under exec rather than read/write/exec on purpose. A more specific grant replaces the one it sits inside, so the exec entry also revokes the write this subtree used to inherit from `~/.cache`: sandboxed code can run the cached package manager but can no longer replace it with something the host will execute later. That makes this a tightening of the default posture, not a relaxation.

Two consequences worth knowing. Corepack can no longer download a *new* package manager version from inside the sandbox, so run `corepack install` on the host after changing a `packageManager` field. And defaults are skipped when the path does not exist, so on a machine that has never run Corepack the first sandboxed invocation still fails and the next one succeeds. Grant `--allow-rwx ~/.cache/node/corepack` if you would rather let the sandbox fetch releases itself.
