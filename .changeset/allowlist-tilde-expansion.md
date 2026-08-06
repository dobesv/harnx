---
harnx: patch
---
Sandbox allowlist entries written with a leading `~` now resolve against the home directory. Config files and tool-server arguments are read without a shell, so `--allow-read ~/.config/foo` arrived literally and was treated as a relative path, producing `<cwd>/~/.config/foo`. That directory does not exist, so the sandbox failed during setup and every `bash_exec` call returned `sandboxing failure: No such file or directory` with no indication of which path was at fault. `~user` is still left alone, since resolving another account's home would need a passwd lookup.
