---
title: "Atlassian CLI (acli) authentication via harnx-proxy-auth"
date: 2026-05-20
category: integration-issues
problem_type: integration_issue
component: harnx-proxy-auth
root_cause: "acli stores credentials in OS keyring which is inaccessible inside birdcage; requires proxy-based auth injection"
resolution_type: config_change
severity: medium
tags:
  - acli
  - atlassian
  - proxy-auth
  - keyring
  - basic-auth
  - jaq
plan_ref: issue-597-acli-proxy-auth-docs
---

## Problem

Atlassian CLI (`acli`) stores API tokens in the OS keyring (libsecret on Linux), which is inaccessible inside the harnx bash birdcage. Agents running `acli` commands inside birdcage cannot authenticate against Atlassian APIs without credential injection.

## Symptoms

- `acli jira workitem view FDEV-1` inside birdcage fails with authentication errors
- Keyring access via D-Bus is blocked by sandbox isolation
- `DBUS_SESSION_BUS_ADDRESS="" acli ...` bypasses keyring lookup but has no fallback auth mechanism

## Investigation Steps

1. Examined `acli` auth mechanism: uses `Authorization: Basic base64(email:api_token)` for Jira/Confluence APIs.

2. Tested whether fake token at login time works: `echo "bad_token" | acli jira auth login --site ... --email ... --token` fails immediately with `Unauthorized`. Concluded: real token required for initial login on host.

3. Verified proxy intercept works without keyring: `DBUS_SESSION_BUS_ADDRESS="" acli jira workitem view FDEV-1` makes HTTP request (returns "Issue does not exist" not local error). Proxy can intercept and replace `Authorization` header.

4. Tested jaq `if-then-end` behavior: confirmed implicit `else .` — filter returns input unchanged when condition false. Combined hooks via `hook1 | hook2` work correctly.

5. Verified `endswith(".atlassian.net")` security: leading dot prevents suffix spoofing (`evilatlassian.net` does NOT match).

## Root Cause

`acli` has two credential sources:
1. **Config metadata** (`~/.config/acli/jira_config.yaml`): stores site, cloud_id, email — accessible in birdcage if path allowed
2. **API token**: stored in OS keyring via libsecret — inaccessible inside birdcage

Without keyring access, `acli` has no mechanism to retrieve credentials. However, it still constructs and sends HTTP requests. The MITM proxy can intercept these requests and inject the correct `Authorization` header.

## Solution

### Step 1 — Login on host (one-time)

```sh
acli jira auth login \
  --site https://<site>.atlassian.net \
  --email <your-email@example.com> \
  --token   # reads token from stdin
```

This writes profile metadata to `~/.config/acli/jira_config.yaml` and stores the token in the OS keyring.

### Step 2 — Export real credentials as environment variables

```sh
export ATLASSIAN_API_TOKEN="<your-real-api-token>"
export ATLASSIAN_EMAIL="<your-email@example.com>"
```

### Step 3 — Configure proxy hook in config.yaml

```yaml
hooks:
  entries:
  - event: PreToolUse
    type: claude-command-persistent
    matcher: "bash_exec|bash_spawn"
    command: >-
      harnx-proxy-auth
      --hook 'if (.host | endswith(".atlassian.net"))
          then .headers.authorization = "Basic \( [(env.ATLASSIAN_EMAIL // ""), (env.ATLASSIAN_API_TOKEN // "")] | join(":") | @base64 )"
          end'
```

The jaq filter:
1. Matches any host ending in `.atlassian.net`
2. Constructs Basic auth: `base64(email:token)`
3. Injects `Authorization` header into request

### Security Consideration

`endswith(".atlassian.net")` prevents DNS-level spoofing but forwards credentials to ANY `*.atlassian.net` tenant. For single-workspace scoping, use explicit equality:

```yaml
--hook 'if .host == "mysite.atlassian.net" then ... end'
```

## Why This Works

1. **acli makes HTTP requests regardless of keyring**: The CLI constructs the HTTP request even when keyring is inaccessible. It just lacks the auth header.

2. **Proxy intercepts at TLS layer**: `harnx-proxy-auth` runs as MITM proxy, intercepting all HTTPS traffic from birdcage processes.

3. **Header replacement happens in-flight**: The proxy's jaq filter modifies the request object before forwarding, injecting the correct `Authorization` header built from environment variables.

4. **No keyring dependency inside birdcage**: Credentials come from environment variables set on host, not from keyring access inside the sandbox.

## Prevention Strategies

### Test Cases

- Verify `acli jira workitem view <ISSUE>` works inside birdcage with proxy configured
- Verify `endswith(".atlassian.net")` does not match `evilatlassian.net`
- Verify unset environment variables produce `401 Unauthorized` (not a crash)

### Configuration Checklist

- [ ] `acli auth login` run on host with real token
- [ ] `~/.config/acli/` in birdcage read paths
- [ ] `ATLASSIAN_API_TOKEN` and `ATLASSIAN_EMAIL` exported in environment
- [ ] Proxy hook configured with correct host matcher
- [ ] Consider explicit host equality for single-workspace auth scoping

## Related Issues

- **Parent solution**: [hudsucker-persistent-hook-proxy-2026-05-16.md](../proxy-hooks/hudsucker-persistent-hook-proxy-2026-05-16.md) — MITM proxy mechanics and hook protocol
- **Plan**: issue-597-acli-proxy-auth-docs
