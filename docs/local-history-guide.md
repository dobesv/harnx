# Local History and Rollback

harnx provides automatic, transparent local history for file-modifying operations. Whenever agent modifies file or runs command that changes filesystem, harnx captures snapshot of repository state. This allows safe, non-destructive rollbacks if agent makes mistake or produces unintended side effects.

## Overview

Local history feature automatically snapshots files before and after every mutating tool call. This provides safety net for AI agents, allowing them to undo their own changes without manual intervention.

Key characteristics:
- **Zero-config**: Automatically activates when MCP roots include git repositories.
- **Transparent**: Snapshots are stored as standard git commit objects with no dedicated refs. Diff responses identify snapshot using `commit <sha>` line at top.
- **Safe Rollback**: `rollback_file` tool can restore repository to any prior snapshot using forward commits, ensuring no history is ever lost.

## How Snapshots Work

Snapshots are triggered by mutating MCP tools. Depending on tool, harnx captures repository state at different points:

| Tool | Server | Snapshot Timing |
|------|--------|-----------------|
| `write_file` | File System | Before and After |
| `edit_file` | File System | Before and After |
| `exec` | Bash | Before and After command execution |
| `spawn` | Bash | Before process starts |
| `wait` | Bash | After process completes |

Each snapshot is written as anonymous git commit object in repository object store. No ref is updated. When tool returns diff, first lines use `git show`-style header:

```text
commit <sha>
    <snapshot label>
```

Use SHA from that `commit <sha>` line as rollback target.

## The `rollback_file` Tool

`rollback_file` restores repository to prior harnx history snapshot by creating new reversible commit. Pass `commit_id` from `commit <sha>` line at top of prior tool response diff.

Rollback process:
1. Capture `before rollback` snapshot of current working tree.
2. Restore files from target harnx snapshot tree.
3. Capture new rollback snapshot chained from step 1.
4. Hard reset working tree to new rollback snapshot commit.

Result: rollback itself is reversible because current state is preserved in `before rollback` snapshot.

## What Gets Snapshotted

harnx uses `git ls-files --cached --others --exclude-standard` to determine which files to include.

- **Included**: All tracked files and untracked-but-not-ignored files.
- **Exclusions**: Respects `.gitignore`, `.git/info/exclude`, and system-level excludes.
- **Submodules**: Handled according to standard `git ls-files` behavior.

### Critical Caveats
- **Loss of Metadata**: File mode bits (executable bits) and symlinks are **NOT preserved**. All entries are written as plain blobs.
- **Symlinks to Regular Files**: When restored, symlinks become regular files containing link path.
- **Executable Bits**: Restored files lose executable permission. Manually `chmod +x` or restore attributes if required.

## What Does NOT Get Snapshotted

- **Non-repo paths**: Files outside discovered git repository are silently skipped. Files in `/tmp` or home directories are only protected if inside git repo.
- **Ignored files**: Anything matched by `.gitignore` is excluded by design.
- **`.git` directory**: Repository metadata itself is never snapshotted.

## Limits and Safety Guards

To ensure performance and prevent disk exhaustion, harnx enforces several safety guards. If limit is exceeded, warning is logged to stderr, but primary tool call still succeeds.

| Limit | Default | Override | Behavior when exceeded |
|-------|---------|----------|------------------------|
| File count per snapshot | 10,000 | `HARNX_HISTORY_MAX_FILES` | Snapshot skipped; warning logged. |
| Per-file size | 10 MiB | `HARNX_HISTORY_MAX_FILE_BYTES` | Oversized file skipped; snapshot continues. |
| Total bytes per snapshot | 100 MiB | `HARNX_HISTORY_MAX_TOTAL_BYTES` | Snapshot truncated at cutoff; partial tree committed. |
| Diff size in response | 50,000 bytes | (Fixed) | `[diff truncated]` notice appended. |

## Storage Location

Snapshots are stored directly within repository `.git/` directory. They are loose commit objects in standard git object store with no dedicated refs.

Unreferenced snapshot commits are cleaned up by normal `git gc` pruning rules. Default `gc.pruneExpire` is typically 2 weeks, so old unreachable history expires naturally. harnx also periodically fires `git gc --auto --quiet` after snapshots to let git maintain object store in background.

## Caveats and Gotchas

- **Metadata Loss**: As noted, symlinks and executable bits are lost. This can break `node_modules/.bin/` or shell scripts.
- **Repo-wide Rollback**: Rollback affects whole repo, not single file.
- **Overwriting Changes**: `rollback_file` overwrites uncommitted manual changes in working tree. These changes are captured in `before rollback` snapshot and are recoverable as long as commit remains in object store.
- **Performance**: Large repositories (thousands of files) may experience latency during mutating operations as snapshots are captured.
- **Git Dependency**: Requires `git` on `$PATH`. If missing, history is silently disabled.
- **Mid-session Repos**: Repo discovery occurs at startup. If you `git init` new repo while harnx is running, it will not be snapshotted until harnx restarts.

## Inspecting and Managing History

You can inspect harnx history using standard git commands:

```sh
# List harnx-authored commits reachable from refs
git log --all --author=harnx-history@localhost --oneline

# Show commit details for one snapshot
git show <snapshot-sha>

# Show what changed between two snapshots
git diff <before-sha> <after-sha>

# Search snapshots by author and message
git log --all --author=harnx-history@localhost --decorate --stat

# Manually inspect snapshot tree
git ls-tree -r <snapshot-sha>

# Let git prune unreachable old snapshots using normal policy
git gc --auto
```

## Privacy Considerations

- **Secret Exposure**: Diffs are sent to AI assistant in tool responses. If you write file containing credentials, secret will be visible in assistant context.
- **Local Storage**: Full file contents are committed to local `.git/objects/` store. Data never leaves machine unless you manually expose repository objects.
