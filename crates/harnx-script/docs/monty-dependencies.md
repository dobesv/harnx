# Monty dependency audit

Enabling BashKit `=0.20.2`'s `python` feature adds `monty =0.0.19`,
`monty-types =0.0.19` and `get-size2 =0.10.1`. Monty runs in-process in Rust;
it doesn't embed or launch host CPython. This audit describes PR5's lockfile on
2026-11-04, compared with PR4 head `a314adb79`.

## Resolved and compiled dependencies

Cargo resolves 52 new package/version pairs. On the Linux host, 44 appear in
`cargo tree -p harnx-script`; eight are inactive:

- `pyo3`, `pyo3-build-config`, `pyo3-ffi`, `pyo3-macros` and
  `pyo3-macros-backend` 0.28.3 are lockfile-only optional dependencies through
  `jiter`. Monty enables `jiter/num-bigint`, not `jiter/python`. No libpython
  linking or host Python detection runs in this feature set.
- `embedded-io` 0.4.0 and 0.6.1 are inactive optional `postcard` dependencies.
- `atomic-polyfill` 1.0.3 is a target-specific `heapless` dependency, inactive on
  the host targets.

The 44 active additions are:

| Group | Packages |
| --- | --- |
| VM | `monty`, `monty-types`, `monty-macros` 0.0.19 |
| Parser | `ruff_python_ast`, `ruff_python_parser`, `ruff_python_stdlib`, `ruff_python_trivia`, `ruff_source_file`, `ruff_text_size` 0.0.3 |
| Memory tracking | `get-size2` 0.10.1, `get-size-derive2` 0.10.3, `ordermap` 1.2.2, `thin-vec` 0.2.21 |
| Data/collections | `jiter` 0.15.0, `postcard` 1.1.3, `cobs` 0.3.0, `heapless` 0.7.17, `hash32` 0.2.1, `bitvec` 1.1.1, `funty` 2.0.0, `radium` 0.7.0, `tap` 1.0.1, `wyz` 0.5.1 |
| Parsing helpers | `fancy-regex` 0.17.0, `speedate` 0.17.0, `lexical-parse-float` 1.0.6, `lexical-parse-integer` 1.0.6, `lexical-util` 1.0.7, `itertools` 0.15.0 |
| Unicode | `unicode-general-category` 1.1.0, `unicode_names2` 1.3.0, `unicode_names2_generator` 1.3.0, `rand_chacha` 0.3.1 |
| Macro helpers | `attribute-derive` 0.10.5, `attribute-derive-macro` 0.10.5, `collection_literals` 1.0.3, `derive-where` 1.7.0, `interpolator` 0.5.0, `is-macro` 0.3.8, `manyhow` 0.11.4, `manyhow-macros` 0.11.4, `proc-macro-utils` 0.10.0, `quote-use` 0.8.4, `quote-use-macros` 0.8.4 |

Existing shared dependencies also gain features (including serde support).
Monty's `fancy-regex` 0.17.0 and BashKit's 0.19.2 coexist. BashKit disables Python
`re` imports despite the regex implementation being linked into Monty.

## Build scripts and a noexec registry

Five newly active packages have build scripts. Source paths below are relative
to Cargo's unpacked registry directory. Cargo compiles build scripts and proc
macros into the executable target directory, not the registry source directory.

| Source | Behavior |
| --- | --- |
| `heapless-0.7.17/build.rs` | Emits target atomic cfgs. `rustc_version::version_meta()` launches toolchain `rustc -vV`, not a registry executable. |
| `radium-0.7.0/build.rs` | Emits target atomic cfgs. No subprocess. |
| `jiter-0.15.0/build.rs` | No-op with current features. Its `pyo3_build_config::use_pyo3_cfgs()` branch is gated on the inactive `python` feature. |
| `unicode-general-category-1.1.0/build.rs` | Generates lookup tables in `OUT_DIR` using Rust code. No subprocess. |
| `unicode_names2-1.3.0/build.rs` | Calls the `unicode_names2_generator` Rust library to generate tables in `OUT_DIR`. No subprocess or bundled executable. |

No new active build script executes a bundled registry binary. A noexec Cargo
registry needs no workaround for this feature. Existing workspace Protobuf
builds still need an executable `PROTOC` outside the registry. Enabling
`jiter/python` in a later change would activate a different build path; repeat
this check rather than assuming the PyO3 packages remain inactive.

## Rust versions

Monty, monty-types and monty-macros declare Rust **1.95** and edition 2024.
Ruff's six parser crates declare 1.94; jiter declares 1.88. The highest declared
minimum in the new active subtree is 1.95, not the earlier BashKit-only estimate
of 1.85. The workspace's pinned Rust **1.98.1** satisfies these declarations.
Verification uses that pinned compiler; this audit doesn't claim a separate
build on Rust 1.95.

## Licenses

Monty and Ruff are MIT. Other additions declare MIT, Apache-2.0, or their dual
license. `unicode_names2` declares
`(MIT OR Apache-2.0) AND Unicode-DFS-2016`; preserve its Unicode data notice
(`unicode_names2-1.3.0/data/LICENSE-UNICODE`) when distributing those tables.
`is-macro` and `unicode-general-category` are Apache-2.0-only. No new dependency
declares a copyleft license. These permissive licenses can be distributed
alongside Harnx's MIT OR Apache-2.0 code with their required notices.

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
