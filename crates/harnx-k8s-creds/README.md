# harnx-k8s-creds

Kubernetes token service and synthetic kubeconfig for sandboxed tools. Host configuration and exec plugins stay on the credential host. Authorized child processes can fetch actual tokens for the selected contexts.

## Tool server mode

Run `harnx-k8s-creds --tool-server --context my-cluster` beside `harnx-bash-tools`. At least one host context is required; repeat `--context` for more. `--kubeconfig <path>` overrides `$KUBECONFIG` or `~/.kube/config`.

It registers `k8s_creds_environment`, a no-argument `{}` tool returning:

```json
{
  "env": {"KUBECONFIG": "<absolute-generated-path>"},
  "filesystem": {"kubeconfig_path": "<absolute-generated-path>"}
}
```

The generated file is a `NamedTempFile` in the OS temporary directory, retained until service shutdown. It contains selected cluster/context/CA data and a curl exec plugin with a local-service capability, not raw source tokens or host plugin commands. Bash must have access to this path and the same loopback network namespace as the token service. `curl` must be on the child `PATH`; the middleware worker can be remote.

The token endpoint resolves static tokens, token files or host exec plugins on demand. Tokens refresh within 60 seconds of expiry; tokens without expiry stay cached. Client-certificate-only auth isn't supported. Returned tokens keep their host permissions and expiry, so protect the capability, generated file and command logs.

Use the opt-in [credential-tools package](../../example_config/credential-tools/README.md): copy `k8s_creds.yaml` and `k8s-creds.py` into your package's `tool_servers/` and `middleware/` directories, then set your context. Its middleware matches exec/spawn, replaces caller `KUBECONFIG`, preserves unrelated env, then calls next. Keep your existing `bash.yaml`; no new legacy hook configuration is needed.

NATS is the default tool transport. `--mcp-stdio` or `--mcp-http` selects MCP; HTTP defaults to port `3011`. Shared `--name`, `--enable-tool`, metrics and healthz flags are supported.

## Standalone sandbox-run usage

Run with `harnx-sandbox-run` using native MCP stdio environment hooks:

```bash
harnx-sandbox-run \
  --hook environment harnx-k8s-creds --context my-cluster \; \
  -- kubectl get pods
```

See [Kubernetes credential setup](../../docs/k8s-creds.md) for deployment identities, transport flags and file lifetime.
