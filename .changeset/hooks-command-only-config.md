---
pantheon: patch
coding: patch
---
Convert the pantheon and coding `bash.yaml` proxy-auth hooks to the command-only
config model. Hook entries now specify only `command` (plus optional
`status_message` and `async`); the native `harnx-proxy-auth` hook self-declares
its event and matcher, so those fields are no longer set in the package config.
