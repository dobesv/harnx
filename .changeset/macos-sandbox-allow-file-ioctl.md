---
harnx: patch
---
Fix interactive TUIs (claude, gemini, bash readline) failing inside `harnx-sandbox-run` on macOS.

birdcage 0.8.1's default Seatbelt profile omits `(allow file-ioctl)`, which causes `tcsetattr` to return EPERM inside the sandbox. As a result, every TUI launched via `harnx-sandbox-run` (or any consumer of `harnx-sandbox-common`) silently loses raw mode: arrow keys leak as literal `^[[A`/`^[OB`, terminal DA1 responses appear in input fields, and trust/confirmation prompts become unnavigable.

birdcage's public `Exception` API only grants path/env/network exceptions — there's no surface for adding operation-level rules like `file-ioctl`, so the macOS sandbox path is now implemented in-tree as `harnx_sandbox_common::macos_sandbox::MacSandbox`. The new profile mirrors birdcage's macOS rule generation (identical deny-then-allow ordering, identical subpath escaping) with one extra line in the default header: `(allow file-ioctl)`. Linux continues to use birdcage unchanged.
