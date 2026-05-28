---
harnx-proxy-auth: minor
---
Add `--env` flag to `harnx-proxy-auth` for sentinel credential injection.

The new `--env '<jaq-script>'` flag generates session-unique fake credential
tokens at startup and injects them into bash tool calls. Hook scripts can then
match on those sentinel values and replace them with real credentials, keeping
real tokens out of tool call arguments, logs, and process listings.

`--env` scripts now receive `{}` as input, so field assignment patterns like
`.GITHUB_TOKEN = "ghs_\($fake_base64_key)"` work directly.

Hook filters can also use sentinel jaq variables directly:
- `$fake_uuid_key`
- `$fake_base64_key`
- `$fake_url_base64_key`
- `$fake_hex_key`
- `$fake_email`

**New jaq helpers** usable in both `--env` and `--hook` scripts:
- `bearer(token)` — returns `"Bearer <token>"`
- `basic(user; pass)` — returns `"Basic <base64(user:pass)>"`

Example — GitHub token injection:
```sh
harnx-proxy-auth \
  --env 'if (env.GITHUB_TOKEN // env.GH_TOKEN) then .GITHUB_TOKEN = "ghs_\($fake_base64_key)" else . end' \
  --hook 'if (.host == "api.github.com") and (.headers.authorization == "Bearer ghs_\($fake_base64_key)")
      then .headers.authorization = bearer(env.GITHUB_TOKEN // env.GH_TOKEN)
      else . end'
```

Example — Atlassian Basic auth:
```sh
harnx-proxy-auth \
  --env 'if (env.ATLASSIAN_API_TOKEN and env.ATLASSIAN_EMAIL) then .ATLASSIAN_API_TOKEN = $fake_uuid_key | .ATLASSIAN_EMAIL = $fake_email else . end' \
  --hook 'if (.host | endswith(".atlassian.net")) and .headers.authorization == "Basic \([$fake_email, $fake_uuid_key] | join(":") | @base64)"
      then .headers.authorization = basic(env.ATLASSIAN_EMAIL; env.ATLASSIAN_API_TOKEN)
      else . end'
```
