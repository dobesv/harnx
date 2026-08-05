# Package System

The harnx package system lets you install agent/MCP/client config bundles from git repositories or OCI registries using `harnx-pkg`.

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
    tool_servers/          # .yaml files — tool server configs (native & bridged MCP)
    clients/               # .yaml files — client configs
  my-agents.patch.yaml     # Optional local overrides (sibling to the dir)
```

## Namespacing

Package agents and servers are automatically namespaced to avoid collisions with top-level configs and with each other.

| What | On-disk name | Runtime name | Tool name visible to agent |
|------|-------------|-------------|----------------------------|
| Agent `coder.md` in `my-pkg` | `packages/my-pkg/agents/coder.md` | `my-pkg/coder` | `my-pkg__coder_session_prompt` |
| Package tool server `fs.yaml`, used by an agent in `my-pkg` | `packages/my-pkg/tool_servers/fs.yaml` | `my-pkg__fs` | `fs_read` |
| Package tool server `fs.yaml`, used by an agent outside `my-pkg` | `packages/my-pkg/tool_servers/fs.yaml` | `my-pkg__fs` | `my-pkg__fs_read` |
| Top-level tool server `fs.yaml` | `tool_servers/fs.yaml` | `fs` | `fs_read` |

Package separators become `__` in tool names. Harnx keeps the server/tool separator as `_`.

Add native tool servers through `tool_servers/`. To use an external MCP server, add its bridge configuration there as well; the bridge publishes the MCP tools through the same naming and routing path.

### Naming Convention

The names of agents, servers, and clients are derived from their **filename stems** (extension stripped):

- **Agents**: `agents/coder.md` becomes `coder`.
- **Tool Servers**: `tool_servers/fs.yaml` becomes `fs`.
- **Clients**: `clients/openai.yaml` becomes `openai`.

For package-provided entities, the name is prefixed with the package name: `<package>/<stem>`. For example, `my-pkg/openai`. Any `name:` field inside the configuration file is ignored.

### Within-package `use_tools` references

When an agent inside a package references tools from the same package, write them exactly as you would for a top-level agent — using the bare server name:

```yaml
# packages/my-pkg/agents/coder.md frontmatter:
use_tools:
  - fs_read   # refers to my-pkg's own tool_servers/fs.yaml — no prefix needed
```

When the agent is active, harnx scopes tool discovery to that agent's package: same-package servers are registered under their bare names, so `fs_read` matches correctly. The LLM sees the tool as `fs_read`, not `my-pkg__fs_read`.

To reference a tool from a **different** package, use the full prefixed name:

```yaml
use_tools:
  - fs_read            # own package's fs server
  - other-pkg__db_query     # another package's db server
```

## Local patch files

You can override any package's configuration by creating a patch file next to the package directory:

```
~/.config/harnx/packages/my-agents.patch.yaml
```

### Patch file format

Patch files use **jq filter strings** to modify package configurations. Each entry in `agents` and `clients` is an array of filters. (Patching `tool_servers` is not currently supported.) Each filter receives the full configuration object for the respective entity and must return the modified version.

```yaml
agents:
  - '.model = "claude:claude-3-5-sonnet"'          # applies to every agent
  - 'if .name == "coder" then .fallback_models = ["openai:gpt-4o"] end'

clients:
  - 'if .name == "claude" then .api_key = "sk-..." end'
```

- **Matching**: Use `if .name == "..." then ... end` for exact matching, or `if (.name | test("...")) then ... end` for pattern matching. The `else .` (pass through unchanged) is implicit when omitted.
- **Context**: Patches match against the **bare name** (filename stem), not the qualified `pkg/name` form. This applies to agents and clients.
- **Chaining**: Filters are applied in sequence. If a filter fails, it is skipped with a warning.

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
