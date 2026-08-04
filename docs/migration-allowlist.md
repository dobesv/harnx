# Migrate filesystem allowlists

Filesystem access for `harnx-fs-tools` and `harnx-bash-tools` is now deny-all by default. Existing tool-server YAML, including files shadowed under `~/.config/harnx`, must opt into batches or explicit paths.

Old CLI options are rejected with exit status 1. This loud failure is intentional: stale configuration must not start with a different accessible set. The removed `roots:` YAML field is different because config loading silently ignores unknown fields; replace it even if startup still succeeds.

The interactive `.mcp roots`, `.mcp add-root`, and `.mcp remove-root` commands were removed with the roots protocol.

## Replace old settings

| Old setting | Replacement |
| :--- | :--- |
| `--root` followed by `<PATH>` | `--allow-rwx <PATH>` |
| `--default-root-{cwd}` | `--allow-repo-work` |
| `--extra-{read,write,exec,rwx}` | `--allow-{read,write,exec,rwx}` |
| `--mcp-` + `root` | Removed; configure each tool server's allowlist |
| `HARNX_` + `MCP_ROOTS` | Removed; configure each tool server's allowlist |
| `roots:` in server YAML | Removed and silently ignored on load |
| `HARNX_BASH_` + `EXTRA_{READABLE,WRITABLE,EXEC,RWX}` | `HARNX_TOOLS_ALLOW_{READ,WRITE,EXEC,RWX}` |

The grouped flag notation means each suffix maps directly. For example, old `extra` read access becomes `--allow-read`, and old `extra` rwx access becomes `--allow-rwx`.

## Choose opt-in batches

- `--allow-common-default`: common operating-system commands, libraries, pseudo-filesystems, and temporary directories.
- `--allow-dev-tools`: supported development toolchains, package caches, and tool configuration.
- `--allow-repo-work`: detected Git, Cargo, Node, and Go project paths, Git common directory, and session working directory.
- `--allow-all`: full filesystem request, subject to the `$HOME` ancestor guard.

A typical bash config enables all three scoped batches:

```yaml
command: harnx-bash-tools
args: [--allow-common-default, --allow-dev-tools, --allow-repo-work]
```

A typical filesystem config enables repository and development paths:

```yaml
command: harnx-fs-tools
args: [--allow-repo-work, --allow-dev-tools]
```

## Account for filesystem permissions

Filesystem operations now distinguish read access from write access. Use `--allow-read` for content that agents must inspect without changing. Use `--allow-write` or `--allow-rwx` for writes. Migrating an old filesystem root to `--allow-rwx` preserves its former read/write behavior.

Write and execute grants imply read access. The `$HOME` ancestor guard prevents allow inputs from making `$HOME` or its ancestors writable or executable.

## Migrate shadowed YAML

Package updates don't overwrite a file that shadows shipped config. Check these locations and replace old settings manually:

- `~/.config/harnx/tool_servers/`
- project-specific package or config overlays
- scripts that invoke tool servers directly
- shell profiles that export old path-list variables

Compare shadowed files with `packages/coding/tool_servers/`, `packages/pantheon/tool_servers/`, or `example_config/tool_servers/` after migration.
