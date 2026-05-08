# Package System

The harnx package system lets you install agent/MCP/ACP/client config bundles from git repositories or OCI registries using `harnx-pkg`.

## Installation

Packages are installed into `~/.config/harnx/packages/` (or the path set by `HARNX_CONFIG_DIR`).

### Install from git

```sh
harnx-pkg add https://github.com/owner/my-agents.git v1.0.0
```

The last path component of the URL becomes the package name (e.g. `my-agents`). Override with `--name`:

```sh
harnx-pkg add https://github.com/owner/my-agents.git v1.0.0 --name my-pkg
```

Install only a subdirectory of the repo:

```sh
harnx-pkg add https://github.com/owner/monorepo.git v2.1.0 --subpath packages/my-agent
```

### Install from a local repo (for development/testing)

```sh
harnx-pkg add file:///path/to/local/repo v1.0.0
```

### Install from OCI registry

```sh
harnx-pkg add ghcr.io/owner/my-agents v1.0.0
# or
harnx-pkg add oci://ghcr.io/owner/my-agents v1.0.0
```

## Semver tag requirement

All version tags must be strict semver with a `v` prefix: `v<major>.<minor>.<patch>`.

- ✅ `v1.2.3`
- ❌ `1.2.3` — missing `v` prefix
- ❌ `v1` — not full semver
- ❌ `v1.2.3-beta` — pre-release not allowed
- ❌ `latest` — not semver

## Listing installed packages

```sh
harnx-pkg list
```

Output example:
```
my-agents                git https://github.com/owner/my-agents.git @ v1.2.3
other-pkg                oci ghcr.io/owner/other-pkg @ v0.5.0
```

## Checking for updates

```sh
harnx-pkg check-for-updates           # check all packages
harnx-pkg check-for-updates my-agents # check a specific package
```

## Updating packages

```sh
harnx-pkg update           # update all packages to latest semver tag
harnx-pkg update my-agents # update a specific package
```

Updates always use the same source type and URL recorded in `manifest.yaml`.

## Removing packages

```sh
harnx-pkg remove my-agents
```

This deletes the package directory. Session transcripts referencing the package's agents are preserved.

## On-disk layout

```
~/.config/harnx/packages/
  my-agents/
    manifest.yaml          # Written by harnx-pkg at install time
    package.yaml           # Optional metadata from the package itself
    agents/                # .md files — one per agent
    mcp_servers/           # .yaml files — MCP server configs
    acp_servers/           # .yaml files — ACP server configs
    clients/               # .yaml files — client configs
  my-agents.patch.yaml     # Optional local overrides (sibling to the dir)
```

## Namespacing

Package agents and servers are automatically namespaced to avoid collisions with top-level configs and with each other.

| What | On-disk name | Runtime name | Tool name |
|------|-------------|-------------|-----------|
| Agent `coder.md` in `my-pkg` | `packages/my-pkg/agents/coder.md` | `my-pkg/coder` | `my-pkg__coder_session_prompt` |
| MCP server `fs.yaml` in `my-pkg` | `packages/my-pkg/mcp_servers/fs.yaml` | `my-pkg__fs` | `my-pkg__fs_read_file` |
| ACP server `helper.yaml` in `my-pkg` | `packages/my-pkg/acp_servers/helper.yaml` | `my-pkg__helper` | `my-pkg__helper_session_prompt` |

The `/` in agent names is replaced with `__` in tool names.

### Within-package `use_tools` references

When an agent inside a package references tools from the same package, write them exactly as you would for a top-level agent — using the bare server name:

```yaml
# packages/my-pkg/agents/coder.md frontmatter:
use_tools:
  - fs_read_file   # refers to my-pkg's own mcp_servers/fs.yaml — no prefix needed
```

When the agent is active, harnx scopes the MCP manager to that agent's package: same-package servers are registered under their bare names, so `fs_read_file` matches correctly. The LLM sees the tool as `fs_read_file`, not `my-pkg__fs_read_file`.

To reference a tool from a **different** package, use the full prefixed name:

```yaml
use_tools:
  - fs_read_file            # own package's fs server
  - other-pkg__db_query     # another package's db server
```

## Local patch files

You can override any package's configuration by creating a patch file next to the package directory:

```
~/.config/harnx/packages/my-agents.patch.yaml
```

### Patch file format

```yaml
agents:
  ".*":                        # regexp matching agent name (within package)
    model: claude-3-5-sonnet   # override model for all agents
    temperature: 0.5
  "coder":                     # match only the "coder" agent
    fallback_models:
      - gpt-4o

clients:
  "claude":                    # regexp matching client name
    api_key: sk-...
```

Agent patches match against the bare agent name (stem), not the qualified `pkg/name` form.

## manifest.yaml schema

Written by `harnx-pkg` at install time. Do not edit manually.

```yaml
name: my-agents
source:
  type: git             # or: oci
  url: https://github.com/owner/my-agents.git
  tag: v1.2.3
  commit: abc123def456...
  subpath: null         # or: "packages/my-agent"
installed_at: "2025-01-15T10:30:00Z"
```

## package.yaml schema

Optionally provided by the package upstream.

```yaml
name: my-agents
description: "Does something useful"
harnx_min_version: "0.30.0"
homepage: https://example.com
license: MIT
version: v1.2.3
```
