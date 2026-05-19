# harnx-k8s-creds

`harnx-k8s-creds` is a persistent Kubernetes credential provider hook for the `harnx` agent harness. It allows tools running inside a sandboxed environment (like `bash_exec` or `bash_spawn`) to access Kubernetes clusters securely without exposing sensitive host-side configuration or long-lived tokens.

## Purpose

By default, exposing a full `KUBECONFIG` file to a sandboxed environment is risky because it often contains sensitive data, including static tokens, client certificates, or references to host-side authentication binaries.

`harnx-k8s-creds` solves this by:
1.  Resolving Kubernetes credentials on the host machine using the standard Kubernetes configuration.
2.  Providing these credentials to the sandbox via a local HTTP server.
3.  Injecting a synthetic, safe `KUBECONFIG` into the sandbox that uses `curl` to fetch short-lived tokens on demand.

## How it Works

`harnx-k8s-creds` operates as a persistent `PreToolUse` hook:

1.  **Startup**: When launched, it binds an HTTP server to a random port on `127.0.0.1` and generates a random UUID bearer token.
2.  **Hook Loop**: It remains running and listens for hook events on `stdin`.
3.  **KUBECONFIG Injection**: For every `PreToolUse` event targeting `bash_exec` or `bash_spawn`, it mutates the tool's environment to include a `KUBECONFIG` environment variable pointing to a temporary, synthetic configuration file.
4.  **Token Serving**: The synthetic `KUBECONFIG` uses a `credential-process` style `exec` plugin that calls `curl` to fetch tokens from the host-side server at `/token/<context>`. The server validates the bearer token before returning the current credential for the requested context.

## Installation

To install `harnx-k8s-creds` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-k8s-creds
```

## Hook Configuration

Register `harnx-k8s-creds` as a persistent hook in your `harnx.yaml` or agent front-matter. You must specify which contexts you want to make available.

### Single-Context Example

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command-persistent
      matcher: "bash_exec|bash_spawn"
      command: harnx-k8s-creds --context my-cluster
```

### Multi-Context Example

```yaml
hooks:
  entries:
    - event: PreToolUse
      type: claude-command-persistent
      matcher: "bash_exec|bash_spawn"
      command: harnx-k8s-creds --context cluster-a --context cluster-b
```

**Note**: You must use `type: claude-command-persistent` to ensure the credential server stays alive across multiple tool calls.

## CLI Flags

*   `--context <name>`: **Required (repeatable)**. The name of the Kubernetes context to make available in the sandbox.
*   `--kubeconfig <path>`: **Optional**. Path to the host's kubeconfig file. If omitted, it follows the standard fallback chain:
    1.  `$KUBECONFIG` environment variable.
    2.  `$HOME/.kube/config`.

## Security

*   **Loopback-only binding**: The HTTP server binds only to `127.0.0.1` and is not accessible from other machines.
*   **Per-session bearer token**: A random bearer token is generated every time the hook starts, preventing unauthorized access to the token endpoint.
*   **No sensitive files in sandbox**: Neither your host `kubeconfig` nor any long-lived tokens/certificates are ever written to or exposed in the sandbox.
*   **Minimal surface area**: `curl` is the only interface for credentials in the sandbox.

## Note on exec plugins

Many Kubernetes clusters use `exec` authentication plugins (like `aws-iam-authenticator` or `gke-gcloud-auth-plugin`). These plugins run on the **host** machine, managed by `harnx-k8s-creds`. The sandbox never sees your AWS/GCP credentials or the authenticator binaries themselves; it only receives the resulting short-lived Kubernetes token.

## Prerequisite

The `curl` binary must be available in the sandbox's `$PATH`, as the synthetic `KUBECONFIG` relies on it to communicate with the host-side credential server.
