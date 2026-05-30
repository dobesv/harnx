---
harnx: patch
---
Fix README install instructions to list all installable binaries instead of only three, and link each binary to its documentation. Added per-crate READMEs for `harnx-serve`, `harnx-acp-server`, `harnx-mcp-fs`, `harnx-mcp-time`, `harnx-mcp-hooks-proxy`, `harnx-proxy-auth`, and `harnx-sandbox-exec` (in `harnx-sandbox-common`). Also wired the previously-omitted `harnx-k8s-creds` binary into the release workflow, Docker image, and `argc install` so it ships and installs alongside `harnx-aws-creds`, and added the missing `harnx-mcp-hooks-proxy` binary to the Docker image.
