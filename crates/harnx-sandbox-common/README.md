# harnx-sandbox-exec

`harnx-sandbox-exec` is a low-level wrapper that runs a single command inside the
[birdcage](https://github.com/phylum-dev/birdcage) filesystem sandbox with an
explicit allow-list of paths. It is the primitive that higher-level tools such as
`harnx-sandbox-run` and `harnx-bash-tools` build on, and it ships in the
`harnx-sandbox-common` crate.

## Overview

The binary configures a birdcage sandbox from the paths and options you pass on
the command line, then execs the supplied command inside it. Unlike
`harnx-sandbox-run`, it applies no default whitelist — you specify exactly which
paths are readable, writable, and executable. Sandboxing is only available on
Unix-like systems.

For a higher-level wrapper with sensible defaults and hook support, see
[`harnx-sandbox-run`](../harnx-sandbox-run/README.md).

## Installation

To install `harnx-sandbox-exec` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-sandbox-common --bin harnx-sandbox-exec
```

## Usage

```sh
harnx-sandbox-exec [OPTIONS] -- <command> [args...]
```

Everything after `--` is the command to run inside the sandbox.

## CLI Options

| Option | Description |
| :--- | :--- |
| `--write <path>` | Allow read+write access to a path (repeatable). |
| `--read <path>` | Allow read-only access to a path (repeatable). |
| `--exec <path>` | Allow read+execute access to a path (repeatable). |
| `--env VAR[=VALUE]` | Pass `VAR` from the host environment, or set it explicitly with `=VALUE` (repeatable). |
| `--no-network` | Disable networking (networking is allowed by default). |
| `--working-dir <path>` | Set the working directory of the spawned command. |
| `--help`, `-h` | Print the help message. |
