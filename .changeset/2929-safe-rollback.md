---
harnx: patch
---

`rollback_file` no longer deletes files a snapshot never recorded or moves the branch. It restores only the paths recorded in the target snapshot into the working tree, leaving unrelated tracked and untracked files, staged changes, HEAD and the index untouched, in both the file and bash tool servers. A destination that has been replaced by a symlink or directory, or would escape the repository, is refused before anything is written and reported as a recoverable tool error. The success message and tool descriptions now describe the returned id as a snapshot rather than a new commit, and the docs (local history guide, script tools, bash MCP server and both crate READMEs) explain the target-only restore, the empty-tree limitation and the reverse-diff undo for a single change.
