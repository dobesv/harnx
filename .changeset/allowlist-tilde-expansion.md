---
harnx: patch
---
Sandbox allowlist entries now keep the symlinks they were written with. A relative entry is still made absolute against the working directory, but nothing beyond that is resolved; symlinks are followed only when checking a path against a grant.

Grants were previously canonicalised at insertion, which had two consequences. It widened a grant to wherever a symlink pointed, so allowing a link could hand over its target. And it lost the path callers actually use: on merged-`/usr` systems `/lib64` collapsed into the `/usr/lib64` entry already present, so the sandbox never mounted `/lib64` and every dynamically linked binary failed to start, because loaders are named absolutely as `/lib64/ld-linux-x86-64.so.2`. That surfaced as `bash_exec` failing every command with `sandboxing failure: No such file or directory`.

A leading `~` in an allowlist entry now resolves against the home directory too. Config files and tool-server arguments are read without a shell, so `--allow-read ~/.config/foo` arrived literally and was treated as relative to the working directory. `~user` is left alone, since resolving another account's home would need a passwd lookup.
