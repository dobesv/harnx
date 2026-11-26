# Local History and Rollback

harnx provides automatic, transparent local history for file-modifying operations. Whenever agent modifies file or runs command that changes filesystem, harnx captures snapshot of repository state. This allows safe, non-destructive rollbacks if agent makes mistake or produces unintended side effects.

## Overview

Local history feature automatically snapshots files before and after every mutating tool call. This provides safety net for AI agents, allowing them to undo their own changes without manual intervention.

Key characteristics:
- **Zero-config**: Automatically activates when a mutated path belongs to a git repository.
- **Transparent**: Snapshots are stored as standard git commit objects with no dedicated refs. Diff responses identify snapshot using `commit <sha>` line at top.
- **Safe Rollback**: `rollback_file` restores the files recorded in any prior snapshot into the working tree, leaving unrelated paths, staged changes and Git's history untouched.

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

That SHA is the snapshot just after the change.

### Diff levels

`exec`, `spawn`, `wait` and `status` of the bash tools and `write`, `edit`, `insert` and `re_replace` of the file tools take a `diff` option that sets how much of the change they report:

- `none`: no snapshot is taken and nothing is printed, so there is nothing to roll back to. On `wait` and `status` it only prints nothing for that call; the process was or was not snapshotted when it was spawned.
- `summary`: one line per changed repository.
- `files`: the summary line, then git's `--stat` lines for up to 50 files.
- `full`: the diff, headed by the `commit <sha>` line shown above.

A direct tool call defaults to `full`. Inside a `bash` script the default is `summary`, printed on stderr.

A summary line is git's shortstat, then the command that shows the change:

```text
1 file changed, 1 insertion(+), 1 deletion(-): git -C /work/repo diff eb58d8a2f19a 3fd6fdcc1dcb
```

The first id is the snapshot before the change and the second is the snapshot just after it. To undo the change, add `--binary` to the command and pipe it into `git -C <repo> apply -R`:

```sh
git -C /work/repo diff --binary eb58d8a2f19a 3fd6fdcc1dcb | git -C /work/repo apply -R
```

`--binary` makes the patch usable for binary files, and `-C` on both sides makes the undo work from any directory. If your git config sets an external diff program or a textconv filter, also pass `--no-ext-diff --no-textconv`, since their output is not a patch.

That touches only the files the change touched and leaves the branch alone. It undoes the recorded patch when it applies cleanly, which is not necessarily every real-world effect of the command. The two snapshots it compares hold only what the capture limits below allowed, and neither is a durable backup ref: unreachable snapshot objects are eventually pruned by `git gc` (see [Storage Location](#storage-location)).

## The `rollback_file` Tool

`rollback_file` restores the files recorded in a prior harnx history snapshot into the working tree. Pass `commit_id` as a full id or a prefix of at least 7 hex digits. The `commit <sha>` line at top of a full diff and the second id on a summary line are the state after that change; the first id on a summary line is the state before it.

Only the paths recorded in the snapshot are restored, and the restored contents appear as uncommitted working-tree changes. Paths absent from the snapshot are left exactly as they are — including files a file tool's snapshot never saw — and HEAD, refs and the index are never moved: rolling back does not create a branch commit. A snapshot with an empty tree records no paths, so rolling back to it restores nothing; it cannot undo the creation of a file. To undo one change, reverse-apply its diff as described above.

The id the tool returns in its success message is the new after-snapshot of the working tree: an ordinary snapshot id, not a branch commit.

Rollback process:
1. Check every destination the snapshot names, refusing unsafe ones before touching anything.
2. Capture `before rollback` snapshot of current working tree.
3. Restore the files recorded in the target harnx snapshot tree.
4. Capture `after rollback` snapshot chained from step 2.

Result: rollback itself is reversible because current state is preserved in `before rollback` snapshot. A rollback whose destination has been replaced by a symlink or directory, or would escape the repository, is refused before anything is written and reported as a recoverable tool error.

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
- **Target-only Restore**: `rollback_file` restores the paths the snapshot recorded and leaves every other path, staged changes and Git history untouched. A file-tool snapshot holds only the file it wrote, so rolling back to one does not touch the repository's other files.
- **Overwriting Changes**: `rollback_file` overwrites the uncommitted working-tree changes of the paths the snapshot records. Those changes are captured in the `before rollback` snapshot and are recoverable only while that commit remains in the object store; unreachable snapshots are subject to `git gc` pruning.
- **Non-atomic Restore**: A multi-path restore writes each recorded path one at a time, so an I/O failure partway through can leave some paths already restored. The destination preflight checks what exists before the restore starts but does not guard against the filesystem changing while it runs. The file tools serialize repository writes with an in-process lock; the bash tool's `rollback_file` takes no such lock.
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
