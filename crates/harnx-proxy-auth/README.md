# harnx-proxy-auth

`harnx-proxy-auth` is a TLS-intercepting authentication proxy designed to inject credentials and run hooks on outgoing HTTP requests. It is particularly useful for providing agents with secure access to protected APIs without exposing long-lived tokens to the agent's environment.

## Overview

The proxy generates a temporary Certificate Authority (CA) to intercept HTTPS traffic. It uses `jq` (via `jaq`) filters to mutate requests—such as adding `Authorization` headers—based on the request host, path, or method.

## Installation

To install `harnx-proxy-auth` from the `harnx` workspace:

```sh
cargo install --path crates/harnx-proxy-auth
```

## CLI Options

| Option | Description |
| :--- | :--- |
| `--hook <JQ_FILTER>` | A `jaq` filter to apply to each request. Receives an object with `host`, `path`, `method`, and `headers`. Can be repeated to pipe multiple filters. |
| `--env <JQ_FILTER>` | A `jaq` filter to generate extra environment variables for hooks, with access to generated sentinel values. |
| `--log-file <PATH>` | Path to write a JSON log line for every proxied request (useful for debugging auth injection). |

## Sentinel Values

When using `--env`, the proxy provides several sentinel variables to the filter:
- `$fake_uuid_key`
- `$fake_base64_key`
- `$fake_url_base64_key`
- `$fake_hex_key`
- `$fake_email`

These can be used to populate headers or environment variables with temporary, session-specific values.
