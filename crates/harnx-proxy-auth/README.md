# harnx-proxy-auth

`harnx-proxy-auth` is a TLS-intercepting authentication proxy designed to inject credentials and run hooks on outgoing HTTP requests. It allows agents to securely access protected APIs without exposing long-lived credentials to the agent's environment.

> **Note**: Hook matching is host-based in the shipped configs—if a hook matches the target host and credentials are available, `harnx-proxy-auth` injects an `Authorization` header even when the caller did not send one. To make an unauthenticated request (such as checking whether a GitHub repository is public), bypass the proxy (`curl -q --proxy '' --noproxy '*'`) with a clean environment and no netrc.

## Modes of Operation

`harnx-proxy-auth` operates as a long-lived tool service (over NATS by default, or MCP via `--mcp-stdio` / `--mcp-http`). It exposes the `proxy_auth` toolset containing the `environment` tool (invoked as `proxy_auth_environment` by in-process middleware or queried directly by `harnx-sandbox-run`). In-process tool middleware and sandbox-run query `environment` to obtain proxy defaults and merged environment variables, retaining the proxy process for the command's lifetime.

## Architecture & Deployment

The proxy server generates an ephemeral Certificate Authority (CA) to intercept HTTPS traffic, evaluates jaq filters on HTTP requests, provisions synthetic configuration files, and manages resident auth hooks (such as `jira-auth-hook.py`).

Because `HTTP_PROXY` and `HTTPS_PROXY` direct traffic to loopback (`http://127.0.0.1:<port>`) and injected CA certificates and synthetic configs reside on the local filesystem, **`harnx-proxy-auth` must share the same network and filesystem namespace as `harnx-bash-tools`**:

- **Managed Supervisor**: Harnx's built-in supervisor starts `harnx-proxy-auth` alongside `harnx-bash-tools` on the bash host automatically when configured in package or tool-server definitions.
- **Unmanaged / Distributed Deployment**: In distributed setups, operators deploy `harnx-proxy-auth --tool-server ...` independently in the same container or host namespace as `harnx-bash-tools`, connecting both to a shared NATS cluster. The worker daemon executing agent turns and in-process middleware can be remote: middleware calls `proxy_auth_environment` over NATS, merges the environment (caller-provided variables win), and forwards the enriched arguments to `bash_exec` / `bash_spawn`.

> **Deployment Warning**: When running multiple bash hosts or proxy instances, connecting to a common NATS URL alone is not sufficient. Each bash host and proxy pair must be isolated using distinct routing scopes (`HARNX_SERVER_SCOPE`) and package/config identities to prevent workers from mixing local filesystem paths (`ca_cert_path`, `temp_file_root`) or loopback proxy ports across hosts.

## Installation

The proxy is built from the `harnx-proxy-auth` crate:

```bash
cargo install --path crates/harnx-proxy-auth
```

## Options & Flags

| Option | Description |
| :--- | :--- |
| `--tool-server` | Optional compatibility flag; tool server mode is now the default. |
| `--mcp-stdio` | Use MCP stdio transport for the tool service (conflicts with `--mcp-http`). |
| `--mcp-http` | Use MCP Streamable HTTP transport for the tool service. |
| `--list-tools [--format text\|json]` | Print the tools this server publishes and exit without connecting to NATS. See [Listing a Server's Tools](../../docs/configuration-guide.md#listing-a-servers-tools---list-tools). |
| `--host <HOST>` | MCP HTTP bind host (default: `0.0.0.0`; requires `--mcp-http`). |
| `--port <PORT>` | MCP HTTP port (default: `3009`; requires `--mcp-http`). |
| `--enable-tool <GLOB>` | Enable only matching tools in tool-server mode (repeatable). |
| `--name <NAME>` | Server name assigned by supervisor. |
| `--hook <HOOK>` | A `jaq` filter, script path (`/`, `./`, `../`), inline shebang script (`#!...`), or package script (`$HARNX_PACKAGE_DIR/...`) applied to each request. Receives `{host, path, method, headers}`. Can be repeated. |
| `--hook-timeout-secs <SECS>` | Per-request timeout in seconds for exec `--hook` stages (default: 30). |
| `--env <JQ_FILTER>` | A `jaq` filter to generate extra environment variables, with access to sentinels, `$temp_file_root`, and `$proxy_port`. |
| `--fs <JQ_FILTER>` | A `jaq` filter generating relative files under a private temp directory (`$temp_file_root`). |
| `--load-yaml <NAME=PATH>` | Load a YAML file into jaq variable `$NAME` before evaluating hooks. |
| `--load-json <NAME=PATH>` | Load a JSON file into jaq variable `$NAME` before evaluating hooks. |
| `--load-raw <NAME=PATH>` | Load a raw text file into jaq variable `$NAME` before evaluating hooks. |
| `--load-exec <NAME=CMD>` | Execute a command and load stdout into jaq variable `$NAME`. |
| `--log-file <PATH>` | Write a JSON log line for every proxied request (useful for debugging auth injection). |

## Tool API: `proxy_auth_environment`

In `--tool-server` mode, `harnx-proxy-auth` provides the `environment` tool in the `proxy_auth` toolset (exposed as `proxy_auth_environment` in agent and middleware contexts).

### Request
The tool takes no arguments:
```json
{}
```

### Response
Returns server-owned proxy environment defaults and provisioned filesystem locations:
```json
{
  "env": {
    "HTTP_PROXY": "http://127.0.0.1:4312",
    "HTTPS_PROXY": "http://127.0.0.1:4312",
    "SSL_CERT_FILE": "/tmp/ca.pem",
    "NODE_EXTRA_CA_CERTS": "/tmp/ca.pem",
    "CURL_CA_BUNDLE": "/tmp/ca.pem",
    "REQUESTS_CA_BUNDLE": "/tmp/ca.pem",
    "GIT_SSL_CAINFO": "/tmp/ca.pem",
    "ACLI_CONFIG_DIR": "/tmp/harnx-fs-root/harnx-fs-acli"
  },
  "filesystem": {
    "ca_cert_path": "/tmp/ca.pem",
    "temp_file_root": "/tmp/harnx-fs-root"
  }
}
```

Middleware merges caller environment keys on top of `env` (`env.update(caller_env)`), ensuring caller overrides take precedence.

## Shipped Configuration Example

In `packages/coding` and `packages/pantheon`:

1. `tool_servers/proxy_auth.yaml` configures the proxy as a tool service:
   ```yaml
   command: harnx-proxy-auth
   description: Long-lived GitHub/Atlassian auth proxy, used by bash middleware
   args:
     - --tool-server
     - --hook
     - 'if .host == "github.com" and (env.GITHUB_TOKEN // env.GH_TOKEN) != null then .headers.authorization = "Basic \(["x-access-token", (env.GITHUB_TOKEN // env.GH_TOKEN)] | join(":") | @base64)" end'
     - --hook
     - 'if (.host == "api.github.com" or .host == "uploads.github.com" or .host == "objects.githubusercontent.com") and (env.GITHUB_TOKEN // env.GH_TOKEN) != null then .headers.authorization = "Bearer \(env.GITHUB_TOKEN // env.GH_TOKEN)" end'
     - --fs
     - '{"harnx-fs-acli/acli/.keep": ""}'
     - --env
     - '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}'
     - --hook
     - $HARNX_PACKAGE_DIR/hooks/jira-auth-hook.py
   ```

2. `tool_servers/bash.yaml` defines in-process middleware with no hooks:
   ```yaml
   middleware:
     - name: proxy-auth
       match: '^bash_(exec|spawn)$'
       script: middleware/proxy-auth.py
       tools: [proxy_auth_environment]
       timeout: 25h
       on_error: deny
   ```

3. `middleware/proxy-auth.py` calls the helper and merges caller env:
   ```python
   additions = tools.proxy_auth_environment()
   if additions.get("is_error") or additions.get("isError"):
       raise RuntimeError("proxy auth environment lookup failed")

   args = tool_input()["arguments"]
   caller_env = args.get("env")
   if not isinstance(caller_env, dict):
       caller_env = {}
   env = additions["env"]
   # Existing caller keys win, including explicit proxy/CA overrides.
   env.update(caller_env)
   args["env"] = env
   respond(call_next(args))
   ```

## Sentinel Values

When using `--env`, the proxy provides several sentinel variables to jaq filters:
- `$fake_uuid_key`: random UUID string
- `$fake_base64_key`: standard base64 of UUID bytes
- `$fake_url_base64_key`: URL-safe unpadded base64 of UUID bytes
- `$fake_hex_key`: lowercase 32-character hex string
- `$fake_email`: UUID with first `-` replaced by `@`
- `$temp_file_root`: per-instance temporary directory path (allocated when `--fs` is provided)
- `$proxy_port`: runtime-allocated proxy port (available after proxy listener binds)

## Standalone Sandbox Run Usage

For `harnx-sandbox-run`, `harnx-proxy-auth` runs directly as an MCP stdio tool service with hook type `environment`:

```sh
harnx-sandbox-run \
  --hook environment harnx-proxy-auth \
    --mcp-stdio \
    --name=proxy_auth \
    --enable-tool=environment \
    --fs '{"harnx-fs-acli/acli/.keep": ""}' \
    --env '{"ACLI_CONFIG_DIR": "\($temp_file_root)/harnx-fs-acli"}' \
    --hook "$SHIM_DIR/jira-auth-hook.py" \
  \; \
  -- acli jira workitem list
```
