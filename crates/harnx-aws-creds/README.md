# harnx-aws-creds

AWS container credential endpoint for sandboxed tools. Host AWS configuration stays on the credential host; authorized child processes fetch actual provider credentials over authenticated loopback HTTP.

## Tool server mode

Run `harnx-aws-creds --tool-server` beside `harnx-bash-tools`. It registers `aws_creds_environment`, a no-argument `{}` tool returning:

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

The service retains its listener and capability until shutdown. `/creds` asks the AWS SDK provider for credentials on demand; expiring providers refresh without restarting the service. Static provider credentials can remain long-lived. The environment tool returns no raw AWS keys and provisions no files.

Use the opt-in [credential-tools package](../../example_config/credential-tools/README.md): copy `aws_creds.yaml` and `aws-creds.py` into your package's `tool_servers/` and `middleware/` directories. Its middleware matches exec/spawn, replaces exactly the three AWS keys above, preserves unrelated caller env, then calls next. No new legacy hook configuration is needed. AWS and bash need the same loopback network namespace; the worker may be remote.

NATS is the default tool transport. `--mcp-stdio` or `--mcp-http` selects MCP; HTTP defaults to port `3010`. Shared `--name`, `--enable-tool`, metrics and healthz flags are supported. `--list-tools [--format text|json]` prints the tools the server publishes and exits without connecting to NATS; see [Listing a Server's Tools](../../docs/configuration-guide.md#listing-a-servers-tools---list-tools). `--profile <NAME>` selects a host AWS profile.

## Standalone sandbox-run usage

Run with `harnx-sandbox-run` using native MCP stdio environment hooks:

```bash
harnx-sandbox-run \
  --hook environment harnx-aws-creds --profile my-profile \; \
  -- aws s3 ls
```

See [AWS credential setup](../../docs/aws-creds.md) for deployment identities, transport flags and security limits.
