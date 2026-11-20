# AWS credentials for bash tools

`harnx-aws-creds` lets sandboxed AWS clients use the host's AWS credential provider without mounting `~/.aws`. Run it beside `harnx-bash-tools`, then use in-process middleware to inject a local credential endpoint before `bash_exec` and `bash_spawn`.

Authorized child processes can retrieve actual AWS credentials from this endpoint. This keeps source credentials out of ambient child environment variables; it does not hide credentials from an authorized AWS client.

## Enable the tool service

Neither credential service is enabled in shipped packages. Copy these opt-in example files into your active package, keeping their relative paths:

- [`tool_servers/aws_creds.yaml`](../example_config/credential-tools/tool_servers/aws_creds.yaml)
- [`middleware/aws-creds.py`](../example_config/credential-tools/middleware/aws-creds.py)

Keep your existing `bash.yaml`. AWS can be enabled without Kubernetes. Optionally add `--profile my-profile` to the service's `args`; otherwise the AWS SDK uses its standard credential chain, including host environment variables, profiles, SSO and role providers.

The YAML starts `harnx-aws-creds --tool-server` and declares middleware matching `^bash_(exec|spawn)$`. The stage calls `aws_creds_environment {}`, merges its `env` over the caller's matching keys, then calls `next`. Unrelated caller keys survive. This preserves the old AWS hook's overwrite behavior, unlike proxy-auth's caller-wins policy. Missing/null `env` means `{}`; non-object `env`, helper failures and malformed helper replies deny the call.

The exact YAML and Python files above are loaded by runtime integration tests for managed and independently deployed services. The tests run both exec and spawn, check precedence and credential refresh, and use a local fixture provider rather than live AWS credentials.

## Tool API and credential lifetime

The toolset is `aws_creds`; its bare tool is `environment`. It accepts only `{}` and returns:

```json
{
  "env": {
    "AWS_CONTAINER_CREDENTIALS_FULL_URI": "http://127.0.0.1:<port>/creds",
    "AWS_CONTAINER_AUTHORIZATION_TOKEN": "<capability>",
    "AWS_REGION": "<resolved-region>"
  },
  "filesystem": {}
}
```

The three AWS keys above replace matching caller values. Other credential sources, including explicit caller `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, aren't cleared; the client's provider precedence still applies. Remove conflicting sources when you want the client to use this endpoint. The tool returns no raw access key, secret key or session token. AWS provisions no files; `filesystem` is empty.

The loopback listener and random capability token stay valid for the tool service's lifetime. AWS SDKs and the AWS CLI fetch credentials from `/creds` on demand, sending the capability token directly as the `Authorization` header (no `Bearer ` prefix). The HTTP handler asks the existing AWS SDK provider for credentials on each request. Expiring provider credentials can refresh without restarting either tool server or changing middleware.

The host provider controls credential scope and expiry. Static host credentials can be returned without an expiration; this service doesn't turn them into short-lived credentials. Protect the capability, returned credentials and command output/logs accordingly.

## Deployment requirements

Both native services must share a network namespace because the credential endpoint binds `127.0.0.1:<random-port>`. The supplied package's local managed supervisor starts them on the same host. If bash runs in another container, remote tool host or Kubernetes sandbox, arrange the shared loopback namespace yourself; NATS connectivity alone isn't enough.

For unmanaged deployment, use the same `HARNX_NATS_URL`, `HARNX_NATS_TOKEN` and `HARNX_SERVER_SCOPE` as the worker. Set `HARNX_SERVER_PACKAGE` to the loaded package name; set `HARNX_SERVER_CONFIG` and `--name` to each service's YAML basename (`aws_creds` or `bash`). Set `HARNX_PACKAGE_DIR` to the package directory on that host. The worker executing middleware may be remote. See [configuration](configuration-guide.md) for tool server setup.

No AWS filesystem sharing is required. Only Kubernetes needs access to a generated kubeconfig.

## Transports and flags

Install with `cargo install --path crates/harnx-aws-creds`, or use the binary included in Harnx releases and Docker images.

| Flag | Purpose |
|------|---------|
| `--profile <NAME>` | Select host AWS profile |
| `--tool-server` | Optional legacy compatibility flag; tool server mode is default |
| `--mcp-stdio` | MCP stdio instead of NATS |
| `--mcp-http` | MCP Streamable HTTP instead of NATS |
| `--host <HOST>` | MCP HTTP bind host, default `0.0.0.0`; requires `--mcp-http` |
| `--port <PORT>` | MCP HTTP port, default `3010`; requires `--mcp-http` |
| `--name <NAME>` | Override toolset registration name |
| `--enable-tool <GLOB>` | Repeatable tool filter, e.g. `environment` |

Metrics and healthz flags use the shared native server implementation. The MCP HTTP listener is separate from the loopback credential endpoint; expose it only on a trusted interface.

## Standalone sandbox-run usage

`harnx-sandbox-run` runs `harnx-aws-creds` directly via native MCP stdio environment hooks:

```bash
harnx-sandbox-run \
  --hook environment harnx-aws-creds --profile my-profile \; \
  -- aws s3 ls
```

The hook type `environment` launches the tool service over MCP stdio, queries its `environment` tool, injects the credential endpoint variables into the sandbox, and holds the service process alive for the duration of the command. See [sandbox-run](sandbox-run.md) and the [opt-in example package](../example_config/credential-tools/README.md).
