---
harnx: patch
---
Snapshot tests no longer break when the version bumps. The TUI welcome
banner's `harnx <semver>` substring is masked to `harnx [VERSION]`
inside the test normalization helpers, so future releases don't need
a `cargo insta accept` chore alongside the version bump.
