# Monty dependency audit

Enabling BashKit `=0.21.1`'s `python` feature adds `bashkit-monty =0.21.1`,
`bashkit-monty-types =0.21.1` and `monty-macros =1.5.0`. Monty runs in-process
in Rust; it doesn't embed or launch host CPython. This audit covers the bump
from BashKit 0.20.2 (Monty 0.0.19, first audited on 2026-11-04) to 0.21.1,
compared with `main` at `2a3936efa` on 2026-11-25. Packages the first audit
covered that this bump leaves in place keep that audit's findings.

## What the bump changed

Cargo resolves 42 new package/version pairs and drops 31. On the Linux host,
`cargo tree -p harnx-script` lists 227 packages that aren't workspace members,
against 217 before.

Removed from the tree:

- The VM: `monty`, `monty-types`, `monty-macros` 0.0.19, and `bashkit` 0.20.2.
- Ruff's parser crates at 0.0.3, which 0.0.14 replaces.
- `get-size2` 0.10.1 and `get-size-derive2` 0.10.3.
- `jiter` 0.15.0, `compact_str` 0.9.1, `fancy-regex` 0.17.0 and 0.19.2, and the
  macro helpers `attribute-derive`, `manyhow` and `quote-use`, each at its
  earlier version.
- `heapless`, `hash32`, `spin`, `lock_api`, `scopeguard`, `byteorder`,
  `semver`, `rustc_version` and `derive-where`.

Added to the tree:

| Group | Packages |
| --- | --- |
| VM | `bashkit-monty`, `bashkit-monty-types` 0.21.1, `monty-macros` 1.5.0 |
| Parser | `ruff_python_ast`, `ruff_python_codegen`, `ruff_python_literal`, `ruff_python_parser`, `ruff_python_stdlib`, `ruff_python_trivia`, `ruff_source_file`, `ruff_text_size` 0.0.14 |
| Memory tracking | `get-size2`, `get-size-derive2` 0.11.1, `compact_str` 0.10.0 |
| Data | `jiter` 0.18.0, `minicbor` 2.3.1, `minicbor-serde` 0.7.2, `serde_bytes` 0.11.19, `fancy-regex` 0.20.2, `shlex` 2.0.1 |
| Stack growth | `stacker` 0.1.25, `psm` 0.1.32, `cc` 1.2.65, `find-msvc-tools` 0.1.9, `ar_archive_writer` 0.5.5, `object` 0.39.1, `nix` 0.32.0, `cfg_aliases` 0.2.2 |
| Lookup tables | `phf`, `phf_generator`, `phf_macros`, `phf_shared` 0.12.1, `char_str` 0.0.4 |
| Macro helpers | `attribute-derive`, `attribute-derive-macro` 0.11.0, `manyhow`, `manyhow-macros` 0.14.1, `quote-use`, `quote-use-macros` 0.9.0, `drop_bomb` 0.1.5 |

BashKit and Monty now bundle the `re` module. `import re` runs inside the VM,
reaches no host service, and its memory is metered like the rest of the heap.

`pyo3`, `pyo3-build-config`, `pyo3-ffi`, `pyo3-macros` and
`pyo3-macros-backend` stay lockfile-only optional dependencies through `jiter`.
`cargo tree -p harnx-script -i pyo3` finds nothing, so no libpython linking or
host Python detection runs in this feature set.

## Build scripts and a noexec registry

Five newly added packages have build scripts. Source paths below are relative
to Cargo's unpacked registry directory. Cargo compiles build scripts and proc
macros into the executable target directory, not the registry source directory.

| Source | Behavior |
| --- | --- |
| `psm-0.1.32/build.rs` | Compiles a small assembly stub for the target with the `cc` crate, which runs the system C compiler. Not a registry executable. |
| `stacker-0.1.25/build.rs` | Compiles one C file with the `cc` crate, on Windows targets only. |
| `object-0.39.1/build.rs` | Emits a cfg after running the toolchain's `rustc --version`. |
| `nix-0.32.0/build.rs` | Emits platform cfgs through `cfg_aliases`. No subprocess. |
| `jiter-0.18.0/build.rs` | No-op with current features. Its `pyo3_build_config::use_pyo3_cfgs()` call is gated on the inactive `python` feature. |

No newly added build script executes a bundled registry binary, so a noexec
Cargo registry needs no workaround for this feature. `psm` and `stacker` do need
a C compiler for the target, as the workspace's `onig` and TLS dependencies
already do. Existing workspace Protobuf builds still need an executable
`PROTOC` outside the registry. Enabling `jiter/python` in a later change would
activate a different build path; repeat this check rather than assuming the
PyO3 packages remain inactive.

## Rust versions

`bashkit-monty`, `bashkit-monty-types`, `monty-macros` and Ruff's eight parser
crates declare Rust **1.96**. `psm`, `ar_archive_writer` and `jiter` declare
1.88. The highest declared minimum in the added subtree is 1.96. The
workspace's pinned Rust **1.100.0** satisfies these declarations. Verification
uses that pinned compiler; this audit doesn't claim a separate build on
Rust 1.96.

## Licenses

BashKit's Monty crates and Ruff are MIT. `minicbor` and `minicbor-serde` are
BlueOak-1.0.0, a permissive license new to this tree. `ar_archive_writer` is
Apache-2.0 WITH LLVM-exception and is a build dependency. `object` is
Apache-2.0 OR MIT. The other additions declare MIT or MIT OR Apache-2.0.
`unicode_names2` is unchanged and still declares
`(MIT OR Apache-2.0) AND Unicode-DFS-2016`; preserve its Unicode data notice
(`unicode_names2-1.3.0/data/LICENSE-UNICODE`) when distributing those tables.
No dependency here declares a copyleft license. These permissive licenses can be
distributed alongside Harnx's MIT OR Apache-2.0 code with their required
notices.

## Recheck after dependency changes

```sh
cargo metadata --locked --format-version 1 > /tmp/monty-metadata.json
cargo tree --locked -p harnx-script -e features
cargo tree --locked -p harnx-script --prefix none
```

Compare metadata package IDs against the base revision, then inspect new
packages' normalized `Cargo.toml`, custom-build targets and build-script source.
The lockfile alone doesn't tell whether PyO3 or a target-specific build script
will execute.
