---
harnx: patch
---

fix(example): sandboxed acli authenticates again — synthetic token written as YAML `!!binary`

`jira-auth-hook.py` wrote the synthetic acli config token as a plain string.
acli stores its token as an encrypted SecretStore blob it expects as a YAML
`!!binary` scalar (the YAML parser base64-decodes it before acli decrypts).
With a plain string, acli failed to decrypt and aborted with "failed to
retrieve authenticated status" **before** ever calling `api.atlassian.com`, so
the proxy's on-the-wire token swap never ran and the sandboxed `acli` reported
unauthorized. This restores the `!!binary` format (originally fixed in the
inline `bash.yaml` config, dropped when the logic moved into the hook) across
all three `jira-auth-hook.py` copies.

The hook now also sources the token per platform automatically — `secret-tool`
on Linux and `security find-generic-password` (login keychain) on macOS —
instead of assuming `secret-tool`; `HARNX_JIRA_TOKEN_CMD` still overrides it.
The Jira docs recipes now use `jira-auth-hook.py` directly rather than an inline
config that re-serialized the token as a plain string.
