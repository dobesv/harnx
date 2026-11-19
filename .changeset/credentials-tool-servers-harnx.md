---
harnx: minor
---
Add long-lived tool service mode to `harnx-aws-creds` and `harnx-k8s-creds`, exposing `aws_creds` and `k8s_creds` toolsets with `aws_creds_environment` and `k8s_creds_environment` tools. Provide opt-in `credential-tools` example package with tool server configs and bash middleware. `harnx-sandbox-run` queries the environment tools over MCP stdio and keeps helper processes alive while the sandboxed command runs.
