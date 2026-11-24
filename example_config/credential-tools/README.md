# credential-tools

Opt-in AWS and Kubernetes credential tool services with in-process middleware for `bash_exec` and `bash_spawn`. Neither service is enabled in shipped packages.

## Enable one provider

Copy the provider YAML and Python script into your active package, preserving their directories:

| Provider | Tool server | Middleware |
|----------|-------------|------------|
| AWS | `tool_servers/aws_creds.yaml` | `middleware/aws-creds.py` |
| Kubernetes | `tool_servers/k8s_creds.yaml` | `middleware/k8s-creds.py` |

Keep your package's existing `bash.yaml`. Each provider YAML declares its own middleware matching `^bash_(exec|spawn)$`; no legacy hooks are needed. Under local worker management, the provider's tool server starts automatically for sessions using the middleware without listing credential tools in the agent's `use_tools`.

AWS works independently. Add `--profile <name>` to `aws_creds.yaml` if needed; otherwise it uses the host AWS SDK credential chain.

Before enabling Kubernetes, replace `my-cluster` in `k8s_creds.yaml` with a context in your host kubeconfig. At least one `--context` is required; repeat it for more contexts. Use `--kubeconfig <path>` to override `$KUBECONFIG` or `~/.kube/config`.

This directory also contains a standalone `package.yaml` and `tool_servers/bash.yaml`. Runtime tests link this exact package under `<config_dir>/packages/credential-tools`, then run real managed and independently deployed services. They verify exec/spawn env precedence, file access and on-demand refresh with host-side fixture credential sources, not live cloud credentials.

## API and precedence

Both binaries use `--tool-server`; NATS is the default transport. Each exposes a strict no-argument `environment {}` tool:

- `aws_creds_environment` returns `env` containing `AWS_CONTAINER_CREDENTIALS_FULL_URI`, `AWS_CONTAINER_AUTHORIZATION_TOKEN` and `AWS_REGION`, plus `filesystem: {}`. AWS provisions no files.
- `k8s_creds_environment` returns `env: {KUBECONFIG: <absolute path>}` and `filesystem: {kubeconfig_path: <same path>}`. This is a `NamedTempFile` in the OS temp directory, retained until service shutdown.

Middleware overwrites exactly those matching caller env keys and preserves unrelated keys. This matches the legacy credential hooks and differs from proxy-auth's caller-wins policy. Missing/null `env` means `{}`; invalid env or helper errors deny the call before next.

The environment tools return endpoint capabilities and file locations, not source AWS keys or Kubernetes tokens. AWS clients and the generated Kubernetes curl exec plugin fetch actual credentials from authenticated loopback HTTP endpoints. Their permissions and expiry come from the host providers; static credentials can remain long-lived. Treat the capabilities, generated kubeconfig and command logs as credentials.

## Deployment

AWS and bash need the same loopback network namespace. Kubernetes also needs its generated kubeconfig readable by bash at the same absolute path, and `curl` on the child `PATH`. The worker executing middleware may be remote.

The supplied native services start on one host under the local managed supervisor when session middleware requires them, without listing credential tools in `use_tools`. For remote bash, separate containers or Kubernetes sandboxes, arrange shared namespaces/mounts yourself; NATS alone doesn't provide them.

Unmanaged services use the worker's `HARNX_NATS_URL`, `HARNX_NATS_TOKEN` and `HARNX_SERVER_SCOPE`. Set `HARNX_SERVER_PACKAGE=credential-tools`, `HARNX_SERVER_CONFIG` and `--name` to each service's YAML basename (`aws_creds`, `k8s_creds` or `bash`), and `HARNX_PACKAGE_DIR` to this package's absolute path on that host. For a copied configuration, substitute its package name/path.

MCP transports are available with `--mcp-stdio` or `--mcp-http`; default MCP HTTP ports are `3010` (AWS) and `3011` (Kubernetes). Shared `--host`, `--port`, `--name` and `--enable-tool` flags apply. MCP HTTP isn't the loopback credential listener.

`--tool-server` is an optional legacy compatibility flag; tool server mode is default. Standalone `harnx-sandbox-run` queries the binaries as native MCP stdio environment tools using `--hook environment <COMMAND> [ARGS...] \;`.

See [AWS credentials](../../docs/aws-creds.md) and [Kubernetes credentials](../../docs/k8s-creds.md) for refresh, security limits and standalone examples.
