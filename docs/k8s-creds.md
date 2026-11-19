# Kubernetes credentials for bash tools

`harnx-k8s-creds` gives sandboxed Kubernetes clients a synthetic kubeconfig backed by a local token service. Host kubeconfig files and auth plugins stay on the credential host; in-process middleware injects `KUBECONFIG` before `bash_exec` and `bash_spawn`.

Authorized child processes receive actual Kubernetes tokens from the service. Tokens have the permissions and expiry of the selected host contexts; the service doesn't reduce those permissions or turn static tokens into expiring ones.

## Enable the tool service

Neither credential service is enabled in shipped packages. Copy these opt-in example files into your active package, keeping their relative paths:

- [`tool_servers/k8s_creds.yaml`](../example_config/credential-tools/tool_servers/k8s_creds.yaml)
- [`middleware/k8s-creds.py`](../example_config/credential-tools/middleware/k8s-creds.py)

Keep your existing `bash.yaml`. Replace `my-cluster` in the service's `args` with a context from your host kubeconfig before enabling it. At least one `--context` is required; repeat the flag to allow several contexts. `--kubeconfig <path>` overrides `$KUBECONFIG` or the default `~/.kube/config`.

The YAML starts `harnx-k8s-creds --tool-server` and declares middleware matching `^bash_(exec|spawn)$`. The stage calls `k8s_creds_environment {}`, overwrites the caller's `KUBECONFIG`, preserves unrelated environment keys, then calls `next`. This matches the legacy hook's precedence, unlike proxy-auth's caller-wins policy. Missing/null `env` means `{}`; non-object `env`, helper failures and malformed helper replies deny the call.

Runtime integration tests load the exact YAML and Python files above in managed and independently deployed configurations. Real exec/spawn children read the synthetic file and run its curl token request. A host-side fixture exec plugin proves near-expiry refresh and valid-token caching without contacting a cluster.

## Tool API and generated file

The toolset is `k8s_creds`; its bare tool is `environment`. It accepts only `{}` and returns:

```json
{
  "env": {"KUBECONFIG": "<absolute-generated-path>"},
  "filesystem": {"kubeconfig_path": "<absolute-generated-path>"}
}
```

The service writes a `tempfile::NamedTempFile` in the OS temporary directory (`std::env::temp_dir()`, affected by platform temp-directory settings). The file is retained for the service's lifetime and removed on graceful shutdown. `filesystem.kubeconfig_path` is location metadata, not a request to copy a file to the worker.

The synthetic kubeconfig contains selected contexts, API server URLs, CA data and a curl exec plugin. It contains a random local-service capability token, not the source Kubernetes token, host plugin command or client private key. `curl` must be available in the execution environment's `PATH`. Its generated command calls `http://127.0.0.1:<port>/token/<context>` with `Authorization: Bearer <capability>`.

Keep the capability and generated file private. An authorized process can use the capability to request tokens for any context selected for this service, not just the current context. Use separate services for separate trust boundaries.

## Token refresh

The `environment` tool returns a stable path; it doesn't fetch Kubernetes tokens. The existing HTTP handler resolves tokens on demand from:

- A static token in the selected host context.
- A token file referenced by that context.
- A host exec credential plugin, such as `aws-iam-authenticator` or `gke-gcloud-auth-plugin`.

Tokens are cached until 60 seconds before their reported expiry. Tokens without an expiry remain cached. Client-certificate-only authentication is not supported by this token service. No cluster connection is made when returning the synthetic kubeconfig.

The token listener remains alive for the entire tool service. Later exec/spawn calls use the same file and capability while token requests refresh expired or near-expiry provider tokens. Protect tokens and command output/logs as credentials.

## Deployment requirements

`harnx-k8s-creds` and `harnx-bash-tools` need the same loopback network namespace and access to the generated file at the same absolute path. The supplied package's local managed supervisor starts them on one host. If bash runs in another container, remote tool host or Kubernetes sandbox, arrange shared namespaces/mounts yourself; NATS connectivity alone doesn't provide them.

For unmanaged deployment, use the same `HARNX_NATS_URL`, `HARNX_NATS_TOKEN` and `HARNX_SERVER_SCOPE` as the worker. Set `HARNX_SERVER_PACKAGE` to the loaded package name; set `HARNX_SERVER_CONFIG` and `--name` to each service's YAML basename (`k8s_creds` or `bash`). Set `HARNX_PACKAGE_DIR` to the package directory on that host. The worker executing middleware may be remote. See [configuration](configuration-guide.md) for tool server setup.

## Transports and flags

Install with `cargo install --path crates/harnx-k8s-creds`, or use the binary included in Harnx releases and Docker images.

| Flag | Purpose |
|------|---------|
| `--context <NAME>` | Required host context; repeatable |
| `--kubeconfig <PATH>` | Override source kubeconfig path |
| `--tool-server` | Optional legacy compatibility flag; tool server mode is default |
| `--mcp-stdio` | MCP stdio instead of NATS |
| `--mcp-http` | MCP Streamable HTTP instead of NATS |
| `--host <HOST>` | MCP HTTP bind host, default `0.0.0.0`; requires `--mcp-http` |
| `--port <PORT>` | MCP HTTP port, default `3011`; requires `--mcp-http` |
| `--name <NAME>` | Override toolset registration name |
| `--enable-tool <GLOB>` | Repeatable tool filter, e.g. `environment` |

Metrics and healthz flags use the shared native server implementation. The MCP HTTP listener is separate from the loopback token endpoint; expose it only on a trusted interface.

## Standalone sandbox-run usage

`harnx-sandbox-run` runs `harnx-k8s-creds` directly via native MCP stdio environment hooks:

```bash
harnx-sandbox-run \
  --hook environment harnx-k8s-creds --context my-cluster \; \
  -- kubectl get pods
```

The hook type `environment` launches the tool service over MCP stdio, queries its `environment` tool, injects the synthetic kubeconfig path into the sandbox, and holds the service process alive for the duration of the command. See [sandbox-run](sandbox-run.md) and the [opt-in example package](../example_config/credential-tools/README.md).
