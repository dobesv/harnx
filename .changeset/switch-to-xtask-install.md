---
harnx: patch
---
Replace the argc-based `install` task with a Rust `xtask` crate. Use `cargo xtask install` (optionally with `--debug` or a list of bin names) to build and install harnx binaries from a local checkout. The bin list is discovered automatically from cargo metadata. Fixes #792.
