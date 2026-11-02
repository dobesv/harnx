---
harnx: patch
coding: patch
pantheon: patch
---
Add fs_write_attachment to copy session attachments into files, including in sandboxes.

Sandbox operators must update the sandbox pod image or manifest to run `harnx-fs-tools` with `--sandbox-transport` (or `HARNX_FS_SANDBOX_TRANSPORT=1`) and replace existing pods so the gateway can deliver attachment bytes via `write_bytes`.

`--sandbox-transport` and `HARNX_FS_SANDBOX_TRANSPORT=1` work only with `--mcp-http`; other modes exit at startup, so don't set the environment variable on shared manifests used by non-HTTP fs servers.
