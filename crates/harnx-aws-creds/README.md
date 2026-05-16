# harnx-aws-creds

`harnx-aws-creds` is a persistent AWS credential provider hook for the `harnx` agent harness. It allows tools running inside a sandboxed environment (like `bash_exec` or `bash_spawn`) to access AWS services securely using the standard AWS container credential provider protocol.

## Purpose

By default, `harnx` strips sensitive AWS environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) from sandbox processes to prevent credential leakage.

`harnx-aws-creds` solves this by:
1.  Resolving AWS credentials on the host machine using the standard AWS credential chain (environment variables, `~/.aws/credentials`, SSO, IAM roles, etc.).
2.  Providing these credentials to the sandbox via a local HTTP server that implements the AWS container credential provider protocol.
3.  Injecting the necessary environment variables into sandboxed bash processes so that AWS SDKs and the AWS CLI work transparently.

## How it Works

`harnx-aws-creds` operates as a persistent `PreToolUse` hook:

1.  **Startup**: When launched, it binds an HTTP server to a random port on `127.0.0.1` and generates a random UUID bearer token.
2.  **Hook Loop**: It remains running and listens for hook events on `stdin`.
3.  **Environment Injection**: For every `PreToolUse` event targeting `bash_exec` or `bash_spawn`, it mutates the tool's environment to include:
    *   `AWS_CONTAINER_CREDENTIALS_FULL_URI`: Points to the internal `/creds` endpoint.
    *   `AWS_CONTAINER_AUTHORIZATION_TOKEN`: The session-specific bearer token.
    *   `AWS_REGION`: The resolved AWS region (defaults to `us-east-1`).
4.  **Credential Serving**: The internal HTTP server responds to `GET /creds` requests with temporary AWS credentials in JSON format. It requires the correct bearer token in the `Authorization` header.

## Installation

To install `harnx-aws-creds` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-aws-creds
```

## Hook Configuration

Register `harnx-aws-creds` as a persistent hook in your `harnx.yaml` or agent front-matter:

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command-persistent
      matcher: "bash_exec|bash_spawn"
      command: harnx-aws-creds --profile my-profile
```

Replace `my-profile` with the name of the AWS profile you want to use, or omit `--profile` entirely to use the default credential chain:

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command-persistent
      matcher: "bash_exec|bash_spawn"
      command: harnx-aws-creds
```

**Note**: You must use `type: claude-command-persistent` to ensure the credential server stays alive across multiple tool calls.

## CLI Flags

*   `--profile <name>`: Optional. Use a specific AWS profile from your host's `~/.aws/config` or `~/.aws/credentials`. If omitted, the default AWS credential chain is used.

## Security

*   **Loopback Binding**: The HTTP server binds only to `127.0.0.1` and is not accessible from other machines.
*   **Per-Session Tokens**: A random bearer token is generated every time the hook starts.
*   **Sandbox Isolation**: The sandbox environment does not need access to your `~/.aws` directory or long-lived credentials. It only sees temporary, scoped credentials.
