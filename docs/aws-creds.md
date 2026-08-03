# harnx-aws-creds

`harnx-aws-creds` is a persistent hook that injects AWS credentials into sandboxed processes. It runs as a sidecar alongside `harnx-bash-tools` or `harnx-sandbox-run`, resolving credentials from the host and making them available inside the sandbox without mounting `~/.aws` or leaking raw key material.

## The Problem It Solves

The birdcage sandbox blocks access to `~/.aws`, and the bash toolset server strips `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` from the child environment by default (they're not in the allowlist). Without `harnx-aws-creds`, any AWS CLI or SDK call inside the sandbox fails with "no credentials found".

## How It Works

1. **Resolves credentials on the host** using the standard [AWS credential chain](https://docs.aws.amazon.com/sdkref/latest/guide/standardized-credentials.html): environment variables → `~/.aws/credentials` → `~/.aws/config` profiles → IAM instance role → SSO → etc.
2. **Starts a local HTTP server** on `127.0.0.1:<random-port>` implementing the [AWS container credentials protocol](https://docs.aws.amazon.com/sdkref/latest/guide/feature-container-credentials.html). This is a standard protocol all AWS SDKs support natively.
3. **Injects three environment variables** into every `bash_exec`/`bash_spawn` tool call via the hook's `PreToolUse` mutation:
   - `AWS_CONTAINER_CREDENTIALS_FULL_URI` — URL of the local server (`http://127.0.0.1:<port>/creds`)
   - `AWS_CONTAINER_AUTHORIZATION_TOKEN` — a per-session UUID bearer token
   - `AWS_REGION` — the region resolved from config

When the sandboxed process calls any AWS SDK, the SDK fetches credentials from the local server. The sandbox process never sees `~/.aws` or raw `AWS_ACCESS_KEY_ID` values — it only knows the loopback URL.

## Installation

```bash
cargo install harnx-aws-creds
```

## Usage

### With `harnx-sandbox-run`

```bash
# Default credential chain (env vars, ~/.aws default profile, IAM role, SSO, etc.)
harnx-sandbox-run --hook claude-command-persistent harnx-aws-creds \; -- aws s3 ls

# Specific named profile from ~/.aws/config
harnx-sandbox-run --hook claude-command-persistent harnx-aws-creds --profile my-profile \; -- aws s3 ls
```

### With `harnx-bash-tools` (via agent config)

In your agent's YAML config:

```yaml
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --matcher "bash_exec|bash_spawn"
        --persistent
        -- harnx-aws-creds
```

With a specific profile:

```yaml
hooks:
  entries:
    - command: >-
        harnx-claude-compatible-hook-server
        --event PreToolUse
        --matcher "bash_exec|bash_spawn"
        --persistent
        -- harnx-aws-creds --profile my-profile
```

## CLI Reference

```text
harnx-aws-creds [OPTIONS]

Options:
  --profile <PROFILE>  AWS profile name to use (default: standard credential chain)
  -h, --help           Print help
```

### `--profile`

Selects a named profile from `~/.aws/config` or `~/.aws/credentials`. Equivalent to setting `AWS_PROFILE=<name>` on the host before running the hook, but scoped to this hook process only.

If omitted, the AWS SDK default credential chain applies:
1. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN` env vars on the host
2. `[default]` profile in `~/.aws/credentials` and `~/.aws/config`
3. IAM instance/task/pod role (EC2, ECS, EKS)
4. AWS SSO (`aws sso login`)
5. Process credentials provider
6. Web identity token

## Security Notes

- The HTTP server binds to **loopback only** (`127.0.0.1`), never to `0.0.0.0`
- The bearer token is a **random UUID generated per session** — it changes every time `harnx-aws-creds` starts
- The token is printed to stderr so operators can see it: `harnx-aws-creds: authorization token: <uuid>`
- Credentials are fetched on-demand from the provider, so short-lived credentials (STS, SSO) are refreshed automatically
- `~/.aws` is never mounted into the sandbox — the credential files remain inaccessible to the sandboxed process

## Relationship to `harnx-bash-tools`

When `harnx-bash-tools` runs a command in the sandbox, it strips most environment variables (including `AWS_*`) from the child process to prevent accidental credential leakage. `harnx-aws-creds` is the approved re-injection path: it uses the hook mechanism to add only the container credential protocol variables, which point to a localhost proxy rather than exposing raw keys.
