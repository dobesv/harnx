---
title: "Atlassian CLI (acli) authentication via harnx-proxy-auth"
date: 2026-05-20
category: integration-issues
problem_type: integration_issue
component: harnx-proxy-auth
root_cause: "Sandboxed processes could read host secrets via DBus; blocking DBus caused acli to fail local credential lookups before reaching the network."
resolution_type: software_fix
severity: medium
tags:
  - acli
  - atlassian
  - proxy-auth
  - keyring
  - basic-auth
  - jaq
  - dbus
  - secret-service
  - sandbox-isolation
  - aes-gcm
plan_ref: acli-sandbox-credential-isolation
---

> **Superseded (2026-06):** The previous workaround (injecting headers via proxy and allowing `~/.config/acli` read access) was incomplete. Research revealed that `acli` performs a keyring lookup *before* making any HTTP requests. If the keyring is inaccessible (e.g., due to sandbox isolation), `acli` fails locally with an "unauthorized" error and never attempts a network connection, preventing the proxy from intervening. Additionally, a major security gap was discovered where sandboxed processes could access the host's DBus session bus to read all OS keyring secrets. This document has been updated to reflect the comprehensive fix.

> **Applies to API-token auth only — not OAuth.** The flow replays the credential as HTTP Basic auth, so it works only when `acli` is authenticated with an **API token** (`acli jira auth login … --token`). An OAuth login (`acli jira auth login --web`) stores a short-lived, rotating bearer token as a gzip-compressed binary blob: the `--load-exec` capture cannot read it (it is not valid UTF-8, so `$atlassian_token` degrades to `null` and the `if $p and $atlassian_token` guards all skip — no synthetic config, no `ACLI_CONFIG_DIR`, no header rewrite), and it could not be replayed as a Basic-auth password even if it could be read. Sandboxed `acli` then fails locally with `unauthorized: use 'acli jira auth login' to authenticate`.

## Problem

Atlassian CLI (`acli`) stores API tokens in the OS keyring (Secret Service/libsecret on Linux). Historically, providing these credentials to sandboxed agents required either granting the sandbox access to the host DBus session (a severe security risk) or finding a way to bypass the local check.

## The Security Gap

The initial Linux sandbox allowlist included `/run` as an executable path. This granted `ExecuteAndRead` access to `/run/user/<uid>/bus` (the DBus session socket). When combined with `XDG_RUNTIME_DIR` being passed through to the sandbox, any sandboxed process could reach the host's Secret Service and read every stored credential (e.g., `secret-tool lookup service acli` succeeded inside the sandbox).

### The Fix
1.  **Least-Privilege `/run` Access**: Removed top-level `/run` and `/var/run` from `SYSTEM_EXEC_PATHS` in `crates/harnx-sandbox-common/src/defaults.rs`. These were replaced with an explicit list of safe subpaths:
    *   `systemd/resolve`
    *   `resolvconf`
    *   `NetworkManager`
    *   `current-system`
    *   `opengl-driver`
    *   `opengl-driver-32`
    *   `udev`
2.  **Environment Isolation**: Excluded `XDG_RUNTIME_DIR` from the `XDG_*` environment variable passthrough in `crates/harnx-mcp-bash/src/server/env.rs`.

## Why acli Breaks (and the Fix)

With DBus access blocked, `acli` fails its local keyring lookup and aborts. However, `acli` has a file-based credential fallback called `SecretStore` used when DBus is unavailable. It stores tokens as encrypted binary blobs in `jira_config.yaml`.

### The AES-GCM Sentinel Blob
The `SecretStore` format is: `random_32_byte_key + ":::" + hex(12_byte_nonce + AES-256-GCM_ciphertext + 16_byte_tag)`. 

**Security Note**: Because the AES-GCM key is embedded directly in the blob, the encryption provides no confidentiality. Security depends entirely on sandbox filesystem isolation and the proxy replacing the credential header.

A fixed, hardcoded "sentinel" blob is used where the plaintext is `harnx-sentinel-token` (using an all-zero 32-byte key and 12-byte nonce). 

**Reproduction Snippet (Python):**
```python
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
import base64
key, nonce = bytes(32), bytes(12)
ct = AESGCM(key).encrypt(nonce, b"harnx-sentinel-token", None)
# Resulting blob used in config:
print(base64.b64encode(key + b":::" + (nonce + ct).hex().encode()).decode())
```

## Generic Proxy Primitives

To support this without `acli`-specific logic in the Rust proxy, `harnx-proxy-auth` provides generic primitives:

*   **`--load-yaml/--load-json/--load-raw <name>=<path>`**: Loads a file at startup as a jaq variable `$name`. If the file is missing or malformed, it becomes `null`.
*   **`--fs <jaq>`**: A transformer for the virtual filesystem. It receives a `files` object (keys = relative paths, values = content) and returns an updated one.
*   **`$temp_file_root`**: A jaq binding containing the path to a private, 0700-protected temporary directory managed by the proxy. Files defined in `--fs` are written here and cleaned up on exit.
*   **`tojson`**: A jaq builtin that emits JSON strings, which are valid YAML for `acli`'s configuration.

## acli Wiring (bash.yaml)

The following configuration in `packages/coding/mcp_servers/bash.yaml` sources the API token from the host OS keyring and synthesizes a private `jira_config.yaml` for `acli` to use:

```yaml
# Load host config to extract current_profile
--load-yaml acli_cfg=~/.config/acli/jira_config.yaml

# Fetch the real token from the host keyring (Linux example)
--load-exec 'atlassian_token=p=$(sed -n "s/^current_profile:[[:space:]]*\"\?\([^\"]*\)\"\?[[:space:]]*$/\1/p" ~/.config/acli/jira_config.yaml); test -n "$p" && secret-tool lookup service acli username "jira:$p"'

# Create synthetic config in a private temp dir
--fs '$acli_cfg.current_profile as $cp |
    (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
    if $p and $atlassian_token then
      . + { "acli/jira_config.yaml": ({ version: 1, current_profile: $cp,
        profiles: [{ site: $p.site, cloud_id: $p.cloud_id, account_id: $p.account_id, auth_type: "api_token",
          token: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA6OjowMDAwMDAwMDAwMDAwMDAwMDAwMDAwMDBhNmM2MzI1MzM1NGQxODBiNjkzYWFjYmRkZjlmYjA2YzFkMGI2NmE0MmQ4Mzc1NmJjM2U5ZjM5ODg4MzRhMGZiM2EzYTRhMWY=" }] } | tojson) }
    end'

# Point acli to the synthetic config
--env '$acli_cfg.current_profile as $cp |
    (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
    if $p and $atlassian_token then .ACLI_CONFIG_DIR = $temp_file_root end'

# Replace the sentinel token with real credentials
--hook '$acli_cfg.current_profile as $cp |
    (first($acli_cfg.profiles[]? | select("\(.cloud_id):\(.account_id)" == $cp))) as $p |
    if $p and $atlassian_token and (.host == "api.atlassian.com" or .host == $p.site)
    then .headers.authorization = basic($p.email // env.ATLASSIAN_EMAIL // ""; $atlassian_token) end'
```

*Note: The `--load-exec` command uses `sed` to extract the `current_profile` from the host config and `secret-tool` (Linux) or `security` (macOS) to fetch the token. The macOS variant is: `security find-generic-password -s acli -a "jira:$p" -w`.*

## Key Primitives

### `--load-exec VAR=<command>`
Executes the command via `sh -c` on the host at startup. The stdout (with a single trailing newline stripped) is captured as the jaq variable `$<VAR>`. If the command fails (non-zero exit, missing tool), the variable is `null`, allowing the flow to degrade gracefully. Captured output is treated as a secret and never logged.

### Synthetic Config Generation
By providing a valid `SecretStore` blob (a sentinel token) in a private `jira_config.yaml`, we satisfy `acli`'s local keyring lookup check without actually granting it access to the host keyring. The sentinel blob's plaintext is `harnx-sentinel-token`.

## Flow Summary

1.  **Host Lookup**: The proxy (outside the sandbox) extracts the current profile from `~/.config/acli/jira_config.yaml` and fetches the real API token from the host keyring.
2.  **Synthetic Config**: The proxy generates a private `jira_config.yaml` containing the sentinel token and points `acli` to it via `ACLI_CONFIG_DIR`.
3.  **Local Decryption**: `acli` decrypts the sentinel token locally (since the key is in the blob, it requires no keyring/DBus).
4.  **Header Construction**: `acli` builds an `Authorization: Basic base64(email:sentinel)` header.
5.  **MITM Interception**: The request is routed through `HTTPS_PROXY` to `harnx-proxy-auth`.
6.  **Credential Swap**: The proxy replaces the `Authorization` header with the real token (sourced from the host keyring) before forwarding the request to Atlassian. The proxy matches only the site in your active `acli` profile (plus `api.atlassian.com`), ensuring credentials are never sent to third-party tenants. The real token never enters the sandbox.

## Why This Works

1.  **Local Checks Bypassed**: By providing a validly encrypted `SecretStore` blob, `acli` is satisfied locally without needing DBus.
2.  **Security via Isolation**: The sandbox cannot reach the host DBus or host config files. The synthetic config exists only in a private, transient temp dir.
3.  **Coordinated Replacement**: The proxy ensures that only "dummy" credentials exist within the sandbox, while the actual API calls are fully authenticated.

## Prevention Strategies

### Test Cases

-   **DBus Blocked**: Verify `secret-tool lookup service acli` fails inside the sandbox.
-   **acli Success**: Verify `acli jira workitem view <ISSUE>` succeeds inside the sandbox.
-   **No Leakage**: Verify the sentinel token (not the real token) is visible if `acli` logs its own headers (e.g., with `-v`).
-   **Graceful No-op**: Verify that if the keyring is locked or `acli auth login` has not been run, no config synthesis occurs and no injection is attempted (clean 401).

### Configuration Checklist

-   [x] `acli auth login` run on host **with an API token** (`--token`, not `--web`/OAuth) to generate `~/.config/acli/jira_config.yaml`.
-   [x] `harnx-proxy-auth` configured with `--load-yaml`, `--load-exec`, `--fs`, and the Atlassian `--hook`.
-   [x] `~/.config/acli` removed from sandbox `--extra-read` paths.
-   [x] `XDG_RUNTIME_DIR` confirmed as excluded from sandbox environment (blocking DBus).

## Related Issues

-   **Plan**: `acli-sandbox-credential-isolation`
-   **Issue**: #754 - Sandbox DBus access security gap
