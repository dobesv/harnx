# Local History and Rollback

harnx provides an automatic, transparent local history system for all file-modifying operations. Whenever an agent modifies a file or runs a command that changes the filesystem, harnx captures a snapshot of the repository state. This allows for safe, non-destructive rollbacks if an agent makes a mistake or produces unintended side effects.

## Overview

The local history feature automatically snapshots your files before and after every mutating tool call. This provides a "safety net" for AI agents, allowing them to undo their own changes without manual intervention.

Key characteristics:
- **Zero-config**: Automatically activates when MCP roots include git repositories.
- **Transparent**: Snapshots are stored as standard git commits in a dedicated namespace (`refs/harnx-history/`), coexisting with your normal git workflow.
- **Safe Rollback**: The `rollback_file` tool can restore a repository to any prior snapshot using forward commits, ensuring no history is ever lost.

## How Snapshots Work

Snapshots are triggered by mutating MCP tools. Depending on the tool, harnx captures the repository state at different points:

| Tool | Server | Snapshot Timing |
|------|--------|-----------------|
| `write_file` | File System | Before and After |
| `edit_file` | File System | Before and After |
| `exec` | Bash | Before and After the command execution |
| `spawn` | Bash | Before the process starts |
| `wait` | Bash | After the process completes |

### Snapshot Metadata
- **Storage**: Each snapshot is a git commit stored under `refs/harnx-history/<session-uuid>`.
- **Session Scoping**: A fresh UUID is generated every time harnx starts. Restarting harnx begins a new history chain.
- **Identity**: Commits use the fixed author/committer: `harnx-history <harnx-history@localhost>`.
- **Visibility**: Tool responses include a `harnx-snapshot: <sha>` line identifying the state *after* the operation.
- **Diffs**: Responses include a unified diff of the changes (truncated at 50 KB with a `[diff truncated]` notice).

## The `rollback_file` Tool

Available on both the File System and Bash MCP servers, `rollback_file` restores a repository to a prior state.

- **Inputs**: 
  - `commit_id`: The 40-character hex SHA from a prior `harnx-snapshot:` line.
  - `repo_path`: Any path within the target repository (used to identify which repo to roll back).
- **Repo-wide Scope**: Despite the name, `rollback_file` restores the **entire repository** to the state captured in the snapshot.
- **Non-destructive**: Rollback is implemented as a forward commit. It captures a "before rollback" snapshot, applies the target state, and captures an "after rollback" snapshot. You can undo a rollback by rolling back to the "before" SHA.
- **Validation**: Only commits within the `refs/harnx-history/*` namespace are valid targets.
- **Branch Preservation**: The tool advances the current branch (or `HEAD` if detached). You will not end up in a detached HEAD state if you were on a branch.

## What Gets Snapshotted

harnx uses `git ls-files --cached --others --exclude-standard` to determine which files to include.

- **Included**: All tracked files and untracked-but-not-ignored files.
- **Exclusions**: Respects `.gitignore`, `.git/info/exclude`, and system-level excludes.
- **Submodules**: Handled according to standard `git ls-files` behavior.

### Critical Caveats
- **Loss of Metadata**: File mode bits (executable bits) and symlinks are **NOT preserved**. All entries are written as plain blobs. 
- **Symlinks to Regular Files**: When restored, symlinks become regular files containing the link path.
- **Executable Bits**: Restored files lose their executable permission. You must manually `chmod +x` or restore these attributes if they are required.

## What Does NOT Get Snapshotted

- **Non-repo paths**: Files outside of a discovered git repository are silently skipped. Files in `/tmp` or home directories are only protected if they are inside a git repo.
- **Ignored files**: Anything matched by `.gitignore` is excluded by design.
- **The `.git` directory**: The repository metadata itself is never snapshotted.
- **History Cache**: The `history_dir()` (where fallback history is stored) is explicitly excluded.

## Limits and Safety Guards

To ensure performance and prevent disk exhaustion, harnx enforces several safety guards. If a limit is exceeded, a warning is logged to stderr, but the primary tool call (e.g., `write_file`) will still succeed.

| Limit | Default | Override | Behavior when exceeded |
|-------|---------|----------|------------------------|
| File count per snapshot | 10,000 | `HARNX_HISTORY_MAX_FILES` | Snapshot skipped; warning logged. |
| Per-file size | 10 MiB | `HARNX_HISTORY_MAX_FILE_BYTES` | Oversized file skipped; snapshot continues. |
| Total bytes per snapshot | 100 MiB | `HARNX_HISTORY_MAX_TOTAL_BYTES` | Snapshot truncated at cutoff; partial tree committed. |
| Diff size in response | 50,000 bytes | (Fixed) | `[diff truncated]` notice appended. |

## Storage Location

Snapshots are stored directly within the repository's `.git/` directory. They use the standard git object store and are pinned by references in `refs/harnx-history/`, ensuring they survive `git gc`.

**Fallback Location**: harnx defines a fallback path (`HARNX_LOCAL_HISTORY_DIR`), typically `$HARNX_CONFIG_DIR/harnx/history`. While this is reserved for future use, it is currently **inactive** for snapshotting; paths outside a git repo are simply skipped today.

## Caveats and Gotchas

- **Metadata Loss**: As noted, symlinks and executable bits are lost. This can break `node_modules/.bin/` or shell scripts.
- **Per-Process History**: Restarting harnx starts a new session. Old session refs remain in the repo but are no longer extended.
- **Repo-wide Rollback**: Rollback affects the whole repo, not just a single file.
- **Overwriting Changes**: `rollback_file` will overwrite uncommitted manual changes in the working tree. These changes are captured in the "before rollback" snapshot and are recoverable from `refs/harnx-history/`.
- **Performance**: Large repositories (thousands of files) may experience latency during mutating operations as snapshots are captured.
- **Git Dependency**: Requires `git` on the `$PATH`. If missing, history is silently disabled.
- **Mid-session Repos**: Repo discovery occurs at startup. If you `git init` a new repo while harnx is running, it will not be snapshotted until harnx restarts.

## Inspecting and Managing History

You can manage harnx history using standard git commands:

```sh
# List all harnx history sessions in a repo
git for-each-ref refs/harnx-history/

# Walk one session's snapshot chain
git log refs/harnx-history/<session-uuid>

# Show what changed between two snapshots
git diff <before-sha> <after-sha>

# Manually inspect a snapshot's tree
git ls-tree -r <snapshot-sha>

# Delete all harnx history refs
git for-each-ref refs/harnx-history/ --format='%(refname)' | xargs -r -n1 git update-ref -d

# Clean up unreachable objects after deleting refs
git gc --prune=now
```

## Privacy Considerations

- **Secret Exposure**: Diffs are sent to the AI assistant in tool responses. If you write a file containing credentials, the secret will be visible in the assistant's context.
- **Local Storage**: Full file contents are committed to the local `.git/objects/` store. This data never leaves your machine unless you manually push the history refs.
